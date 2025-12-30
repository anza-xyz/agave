use {
    crate::{
        encoding::{reverse_48_byte_chunks, swap_g2_c0_c1, Endianness},
        Version,
    },
    blstrs::{G1Affine, G2Affine, Scalar},
    std::convert::TryInto,
};

pub fn bls12_381_g1_multiplication(
    _version: Version,
    input: &[u8],
    endianness: Endianness,
) -> Option<Vec<u8>> {
    if input.len() != 128 {
        return None;
    }

    let p1 = match endianness {
        Endianness::BE => {
            // make zero-copy when possible
            let bytes: &[u8; 96] = input[0..96].try_into().ok()?;
            G1Affine::from_uncompressed(bytes).into_option()?
        }
        Endianness::LE => {
            // to reverse the bytes, we need an owned copy
            let mut bytes: [u8; 96] = input[0..96].try_into().ok()?;
            reverse_48_byte_chunks(&mut bytes);
            G1Affine::from_uncompressed(&bytes).into_option()?
        }
    };

    // blstrs provides specific BE/LE parsers, so we don't need manual reversal.
    let scalar_bytes: &[u8; 32] = input[96..128].try_into().ok()?;
    let scalar = match endianness {
        Endianness::BE => Scalar::from_bytes_be(scalar_bytes).into_option()?,
        Endianness::LE => Scalar::from_bytes_le(scalar_bytes).into_option()?,
    };

    #[allow(clippy::arithmetic_side_effects)]
    let result_proj = p1 * scalar;
    let mut result_affine = result_proj.to_uncompressed();

    if matches!(endianness, Endianness::LE) {
        reverse_48_byte_chunks(&mut result_affine);
    }
    Some(result_affine.to_vec())
}

pub fn bls12_381_g2_multiplication(
    _version: Version,
    input: &[u8],
    endianness: Endianness,
) -> Option<Vec<u8>> {
    if input.len() != 224 {
        return None;
    }

    let p1 = match endianness {
        Endianness::BE => {
            let bytes: &[u8; 192] = input[0..192].try_into().ok()?;
            G2Affine::from_uncompressed(bytes).into_option()?
        }
        Endianness::LE => {
            let mut bytes: [u8; 192] = input[0..192].try_into().ok()?;
            reverse_48_byte_chunks(&mut bytes);
            swap_g2_c0_c1(&mut bytes);
            G2Affine::from_uncompressed(&bytes).into_option()?
        }
    };

    let scalar_bytes: &[u8; 32] = input[192..224].try_into().ok()?;
    let scalar = match endianness {
        Endianness::BE => Scalar::from_bytes_be(scalar_bytes).into_option()?,
        Endianness::LE => Scalar::from_bytes_le(scalar_bytes).into_option()?,
    };

    #[allow(clippy::arithmetic_side_effects)]
    let result_proj = p1 * scalar;
    let mut result_affine = result_proj.to_uncompressed();

    if matches!(endianness, Endianness::LE) {
        swap_g2_c0_c1(&mut result_affine);
        reverse_48_byte_chunks(&mut result_affine);
    }
    Some(result_affine.to_vec())
}

#[cfg(test)]
mod tests {
    use {super::*, crate::test_vectors::*};

    fn run_g1_test(
        op_name: &str,
        func: fn(Version, &[u8], Endianness) -> Option<Vec<u8>>,
        input_be: &[u8],
        output_be: &[u8],
        input_le: &[u8],
        output_le: &[u8],
    ) {
        // Test Big Endian
        let result_be = func(Version::V0, input_be, Endianness::BE);
        assert_eq!(
            result_be,
            Some(output_be.to_vec()),
            "G1 {op_name} BE Test Failed",
        );

        // Test Little Endian
        let result_le = func(Version::V0, input_le, Endianness::LE);
        assert_eq!(
            result_le,
            Some(output_le.to_vec()),
            "G1 {op_name} LE Test Failed",
        );
    }

    fn run_g2_test(
        op_name: &str,
        func: fn(Version, &[u8], Endianness) -> Option<Vec<u8>>,
        input_be: &[u8],
        output_be: &[u8],
        input_le: &[u8],
        output_le: &[u8],
    ) {
        // Test Big Endian
        let result_be = func(Version::V0, input_be, Endianness::BE);
        assert_eq!(
            result_be,
            Some(output_be.to_vec()),
            "G2 {op_name} BE Test Failed",
        );

        // Test Little Endian
        let result_le = func(Version::V0, input_le, Endianness::LE);
        assert_eq!(
            result_le,
            Some(output_le.to_vec()),
            "G2 {op_name} LE Test Failed",
        );
    }

    #[test]
    fn test_g1_multiplication_random() {
        run_g1_test(
            "MUL",
            bls12_381_g1_multiplication,
            INPUT_BE_G1_MUL_RANDOM,
            OUTPUT_BE_G1_MUL_RANDOM,
            INPUT_LE_G1_MUL_RANDOM,
            OUTPUT_LE_G1_MUL_RANDOM,
        );
    }

    #[test]
    fn test_g1_multiplication_zero() {
        run_g1_test(
            "MUL",
            bls12_381_g1_multiplication,
            INPUT_BE_G1_MUL_SCALAR_ZERO,
            OUTPUT_BE_G1_MUL_SCALAR_ZERO,
            INPUT_LE_G1_MUL_SCALAR_ZERO,
            OUTPUT_LE_G1_MUL_SCALAR_ZERO,
        );
    }

    #[test]
    fn test_g1_multiplication_one() {
        run_g1_test(
            "MUL",
            bls12_381_g1_multiplication,
            INPUT_BE_G1_MUL_SCALAR_ONE,
            OUTPUT_BE_G1_MUL_SCALAR_ONE,
            INPUT_LE_G1_MUL_SCALAR_ONE,
            OUTPUT_LE_G1_MUL_SCALAR_ONE,
        );
    }

    #[test]
    fn test_g1_multiplication_minus_one() {
        run_g1_test(
            "MUL",
            bls12_381_g1_multiplication,
            INPUT_BE_G1_MUL_SCALAR_MINUS_ONE,
            OUTPUT_BE_G1_MUL_SCALAR_MINUS_ONE,
            INPUT_LE_G1_MUL_SCALAR_MINUS_ONE,
            OUTPUT_LE_G1_MUL_SCALAR_MINUS_ONE,
        );
    }

    #[test]
    fn test_g1_multiplication_infinity() {
        run_g1_test(
            "MUL",
            bls12_381_g1_multiplication,
            INPUT_BE_G1_MUL_POINT_INFINITY,
            OUTPUT_BE_G1_MUL_POINT_INFINITY,
            INPUT_LE_G1_MUL_POINT_INFINITY,
            OUTPUT_LE_G1_MUL_POINT_INFINITY,
        );
    }

    #[test]
    fn test_g2_multiplication_random() {
        run_g2_test(
            "MUL",
            bls12_381_g2_multiplication,
            INPUT_BE_G2_MUL_RANDOM,
            OUTPUT_BE_G2_MUL_RANDOM,
            INPUT_LE_G2_MUL_RANDOM,
            OUTPUT_LE_G2_MUL_RANDOM,
        );
    }

    #[test]
    fn test_g2_multiplication_zero() {
        run_g2_test(
            "MUL",
            bls12_381_g2_multiplication,
            INPUT_BE_G2_MUL_SCALAR_ZERO,
            OUTPUT_BE_G2_MUL_SCALAR_ZERO,
            INPUT_LE_G2_MUL_SCALAR_ZERO,
            OUTPUT_LE_G2_MUL_SCALAR_ZERO,
        );
    }

    #[test]
    fn test_g2_multiplication_one() {
        run_g2_test(
            "MUL",
            bls12_381_g2_multiplication,
            INPUT_BE_G2_MUL_SCALAR_ONE,
            OUTPUT_BE_G2_MUL_SCALAR_ONE,
            INPUT_LE_G2_MUL_SCALAR_ONE,
            OUTPUT_LE_G2_MUL_SCALAR_ONE,
        );
    }

    #[test]
    fn test_g2_multiplication_minus_one() {
        run_g2_test(
            "MUL",
            bls12_381_g2_multiplication,
            INPUT_BE_G2_MUL_SCALAR_MINUS_ONE,
            OUTPUT_BE_G2_MUL_SCALAR_MINUS_ONE,
            INPUT_LE_G2_MUL_SCALAR_MINUS_ONE,
            OUTPUT_LE_G2_MUL_SCALAR_MINUS_ONE,
        );
    }

    #[test]
    fn test_g2_multiplication_infinity() {
        run_g2_test(
            "MUL",
            bls12_381_g2_multiplication,
            INPUT_BE_G2_MUL_POINT_INFINITY,
            OUTPUT_BE_G2_MUL_POINT_INFINITY,
            INPUT_LE_G2_MUL_POINT_INFINITY,
            OUTPUT_LE_G2_MUL_POINT_INFINITY,
        );
    }

    #[test]
    fn test_invalid_length() {
        // G1 (96) + Scalar (32) = 128 bytes
        assert!(bls12_381_g1_multiplication(Version::V0, &[0u8; 127], Endianness::BE).is_none());
        // G2 (192) + Scalar (32) = 224 bytes
        assert!(bls12_381_g2_multiplication(Version::V0, &[0u8; 223], Endianness::BE).is_none());
    }
}
