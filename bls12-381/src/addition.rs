use {
    crate::{reverse_48_byte_chunks, swap_g2_c0_c1, Endianness, Version},
    blstrs::{G1Affine, G1Projective, G2Affine, G2Projective},
};

pub fn bls12_381_g1_addition(
    _version: Version,
    input: &[u8],
    endianness: Endianness,
) -> Option<Vec<u8>> {
    if input.len() != 192 {
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

    let p2 = match endianness {
        Endianness::BE => {
            let bytes: &[u8; 96] = input[96..192].try_into().ok()?;
            G1Affine::from_uncompressed(bytes).into_option()?
        }
        Endianness::LE => {
            let mut bytes: [u8; 96] = input[96..192].try_into().ok()?;
            reverse_48_byte_chunks(&mut bytes);
            G1Affine::from_uncompressed(&bytes).into_option()?
        }
    };

    let p1_proj: G1Projective = p1.into();
    #[allow(clippy::arithmetic_side_effects)]
    let sum_proj = p1_proj + p2;
    let mut sum_affine = sum_proj.to_uncompressed();

    if matches!(endianness, Endianness::LE) {
        reverse_48_byte_chunks(&mut sum_affine);
    }
    Some(sum_affine.to_vec())
}

pub fn bls12_381_g2_addition(
    _version: Version,
    input: &[u8],
    endianness: Endianness,
) -> Option<Vec<u8>> {
    if input.len() != 384 {
        return None;
    }

    let p1 = match endianness {
        Endianness::BE => {
            let bytes: &[u8; 192] = input[0..192].try_into().ok()?;
            G2Affine::from_uncompressed(bytes).into_option()?
        }
        Endianness::LE => {
            let mut bytes: [u8; 192] = input[0..192].try_into().ok()?;
            reverse_48_byte_chunks(&mut bytes); // Fix Fq endianness
            swap_g2_c0_c1(&mut bytes); // Fix Fq2 ordering
            G2Affine::from_uncompressed(&bytes).into_option()?
        }
    };

    let p2 = match endianness {
        Endianness::BE => {
            let bytes: &[u8; 192] = input[192..384].try_into().ok()?;
            G2Affine::from_uncompressed(bytes).into_option()?
        }
        Endianness::LE => {
            let mut bytes: [u8; 192] = input[192..384].try_into().ok()?;
            reverse_48_byte_chunks(&mut bytes);
            swap_g2_c0_c1(&mut bytes);
            G2Affine::from_uncompressed(&bytes).into_option()?
        }
    };

    let p1_proj: G2Projective = p1.into();
    #[allow(clippy::arithmetic_side_effects)]
    let sum_proj = p1_proj + p2;
    let mut sum_affine = sum_proj.to_uncompressed();

    if matches!(endianness, Endianness::LE) {
        swap_g2_c0_c1(&mut sum_affine);
        reverse_48_byte_chunks(&mut sum_affine);
    }
    Some(sum_affine.to_vec())
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
            "G1 {} BE Test Failed",
            op_name
        );

        // Test Little Endian
        let result_le = func(Version::V0, input_le, Endianness::LE);
        assert_eq!(
            result_le,
            Some(output_le.to_vec()),
            "G1 {} LE Test Failed",
            op_name
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
            "G2 {} BE Test Failed",
            op_name
        );

        // Test Little Endian
        let result_le = func(Version::V0, input_le, Endianness::LE);
        assert_eq!(
            result_le,
            Some(output_le.to_vec()),
            "G2 {} LE Test Failed",
            op_name
        );
    }

    #[test]
    fn test_g1_addition_random() {
        run_g1_test(
            "ADD",
            bls12_381_g1_addition,
            INPUT_BE_G1_ADD_RANDOM,
            OUTPUT_BE_G1_ADD_RANDOM,
            INPUT_LE_G1_ADD_RANDOM,
            OUTPUT_LE_G1_ADD_RANDOM,
        );
    }

    #[test]
    fn test_g1_addition_doubling() {
        run_g1_test(
            "ADD",
            bls12_381_g1_addition,
            INPUT_BE_G1_ADD_DOUBLING,
            OUTPUT_BE_G1_ADD_DOUBLING,
            INPUT_LE_G1_ADD_DOUBLING,
            OUTPUT_LE_G1_ADD_DOUBLING,
        );
    }

    #[test]
    fn test_g1_addition_infinity_edge_cases() {
        // P + Inf
        run_g1_test(
            "ADD",
            bls12_381_g1_addition,
            INPUT_BE_G1_ADD_P_PLUS_INF,
            OUTPUT_BE_G1_ADD_P_PLUS_INF,
            INPUT_LE_G1_ADD_P_PLUS_INF,
            OUTPUT_LE_G1_ADD_P_PLUS_INF,
        );
        // Inf + Inf
        run_g1_test(
            "ADD",
            bls12_381_g1_addition,
            INPUT_BE_G1_ADD_INF_PLUS_INF,
            OUTPUT_BE_G1_ADD_INF_PLUS_INF,
            INPUT_LE_G1_ADD_INF_PLUS_INF,
            OUTPUT_LE_G1_ADD_INF_PLUS_INF,
        );
    }

    #[test]
    fn test_g2_addition_random() {
        run_g2_test(
            "ADD",
            bls12_381_g2_addition,
            INPUT_BE_G2_ADD_RANDOM,
            OUTPUT_BE_G2_ADD_RANDOM,
            INPUT_LE_G2_ADD_RANDOM,
            OUTPUT_LE_G2_ADD_RANDOM,
        );
    }

    #[test]
    fn test_g2_addition_doubling() {
        run_g2_test(
            "ADD",
            bls12_381_g2_addition,
            INPUT_BE_G2_ADD_DOUBLING,
            OUTPUT_BE_G2_ADD_DOUBLING,
            INPUT_LE_G2_ADD_DOUBLING,
            OUTPUT_LE_G2_ADD_DOUBLING,
        );
    }

    #[test]
    fn test_g2_addition_infinity_edge_cases() {
        // P + Inf
        run_g2_test(
            "ADD",
            bls12_381_g2_addition,
            INPUT_BE_G2_ADD_P_PLUS_INF,
            OUTPUT_BE_G2_ADD_P_PLUS_INF,
            INPUT_LE_G2_ADD_P_PLUS_INF,
            OUTPUT_LE_G2_ADD_P_PLUS_INF,
        );
        // Inf + Inf
        run_g2_test(
            "ADD",
            bls12_381_g2_addition,
            INPUT_BE_G2_ADD_INF_PLUS_INF,
            OUTPUT_BE_G2_ADD_INF_PLUS_INF,
            INPUT_LE_G2_ADD_INF_PLUS_INF,
            OUTPUT_LE_G2_ADD_INF_PLUS_INF,
        );
    }

    #[test]
    fn test_invalid_length() {
        // G1 expects 192 bytes
        assert!(bls12_381_g1_addition(Version::V0, &[0u8; 191], Endianness::BE).is_none());
        // G2 expects 384 bytes
        assert!(bls12_381_g2_addition(Version::V0, &[0u8; 383], Endianness::BE).is_none());
    }
}
