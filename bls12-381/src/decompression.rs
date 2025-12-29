use {
    crate::{reverse_48_byte_chunks, swap_g2_c0_c1, Endianness, Version},
    blstrs::{G1Affine, G2Affine},
    std::convert::TryInto,
};

pub fn bls12_381_g1_decompress(
    _version: Version,
    input: &[u8],
    endianness: Endianness,
) -> Option<Vec<u8>> {
    if input.len() != 48 {
        return None;
    }

    let p1 = match endianness {
        Endianness::BE => {
            let bytes: &[u8; 48] = input.try_into().ok()?;
            G1Affine::from_compressed(bytes).into_option()?
        }
        Endianness::LE => {
            let mut bytes: [u8; 48] = input.try_into().ok()?;
            reverse_48_byte_chunks(&mut bytes);
            // Flags are now in the first byte (BE standard) after reversal
            G1Affine::from_compressed(&bytes).into_option()?
        }
    };

    let mut result_affine = p1.to_uncompressed();
    if matches!(endianness, Endianness::LE) {
        reverse_48_byte_chunks(&mut result_affine);
    }
    Some(result_affine.to_vec())
}

pub fn bls12_381_g2_decompress(
    _version: Version,
    input: &[u8],
    endianness: Endianness,
) -> Option<Vec<u8>> {
    if input.len() != 96 {
        return None;
    }

    let p2 = match endianness {
        Endianness::BE => {
            let bytes: &[u8; 96] = input.try_into().ok()?;
            G2Affine::from_compressed(bytes).into_option()?
        }
        Endianness::LE => {
            let mut bytes: [u8; 96] = input.try_into().ok()?;
            reverse_48_byte_chunks(&mut bytes);
            swap_g2_c0_c1(&mut bytes); // Swap c0/c1 for G2
            G2Affine::from_compressed(&bytes).into_option()?
        }
    };

    let mut result_affine = p2.to_uncompressed();
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
        input_be: &[u8],
        output_be: Option<&[u8]>,
        input_le: &[u8],
        output_le: Option<&[u8]>,
    ) {
        // Test Big Endian
        let result_be = bls12_381_g1_decompress(Version::V0, input_be, Endianness::BE);
        match output_be {
            Some(expected) => assert_eq!(
                result_be,
                Some(expected.to_vec()),
                "G1 {} BE Test Failed",
                op_name
            ),
            None => assert!(result_be.is_none(), "G1 {} BE expected failure", op_name),
        }

        // Test Little Endian
        let result_le = bls12_381_g1_decompress(Version::V0, input_le, Endianness::LE);
        match output_le {
            Some(expected) => assert_eq!(
                result_le,
                Some(expected.to_vec()),
                "G1 {} LE Test Failed",
                op_name
            ),
            None => assert!(result_le.is_none(), "G1 {} LE expected failure", op_name),
        }
    }

    fn run_g2_test(
        op_name: &str,
        input_be: &[u8],
        output_be: Option<&[u8]>,
        input_le: &[u8],
        output_le: Option<&[u8]>,
    ) {
        // Test Big Endian
        let result_be = bls12_381_g2_decompress(Version::V0, input_be, Endianness::BE);
        match output_be {
            Some(expected) => assert_eq!(
                result_be,
                Some(expected.to_vec()),
                "G2 {} BE Test Failed",
                op_name
            ),
            None => assert!(result_be.is_none(), "G2 {} BE expected failure", op_name),
        }

        // Test Little Endian
        let result_le = bls12_381_g2_decompress(Version::V0, input_le, Endianness::LE);
        match output_le {
            Some(expected) => assert_eq!(
                result_le,
                Some(expected.to_vec()),
                "G2 {} LE Test Failed",
                op_name
            ),
            None => assert!(result_le.is_none(), "G2 {} LE expected failure", op_name),
        }
    }

    #[test]
    fn test_g1_decompress_random() {
        run_g1_test(
            "RANDOM",
            INPUT_BE_G1_DECOMPRESS_RANDOM,
            Some(OUTPUT_BE_G1_DECOMPRESS_RANDOM),
            INPUT_LE_G1_DECOMPRESS_RANDOM,
            Some(OUTPUT_LE_G1_DECOMPRESS_RANDOM),
        );
    }

    #[test]
    fn test_g1_decompress_infinity() {
        run_g1_test(
            "INFINITY",
            INPUT_BE_G1_DECOMPRESS_INFINITY,
            Some(OUTPUT_BE_G1_DECOMPRESS_INFINITY),
            INPUT_LE_G1_DECOMPRESS_INFINITY,
            Some(OUTPUT_LE_G1_DECOMPRESS_INFINITY),
        );
    }

    #[test]
    fn test_g1_decompress_generator() {
        run_g1_test(
            "GENERATOR",
            INPUT_BE_G1_DECOMPRESS_GENERATOR,
            Some(OUTPUT_BE_G1_DECOMPRESS_GENERATOR),
            INPUT_LE_G1_DECOMPRESS_GENERATOR,
            Some(OUTPUT_LE_G1_DECOMPRESS_GENERATOR),
        );
    }

    #[test]
    fn test_g1_decompress_invalid() {
        run_g1_test(
            "INVALID_CURVE",
            INPUT_BE_G1_DECOMPRESS_RANDOM_INVALID_CURVE,
            None,
            INPUT_LE_G1_DECOMPRESS_RANDOM_INVALID_CURVE,
            None,
        );
        run_g1_test(
            "INVALID_FIELD",
            INPUT_BE_G1_DECOMPRESS_FIELD_TOO_LARGE_INVALID,
            None,
            INPUT_LE_G1_DECOMPRESS_FIELD_TOO_LARGE_INVALID,
            None,
        );
    }

    #[test]
    fn test_g2_decompress_random() {
        run_g2_test(
            "RANDOM",
            INPUT_BE_G2_DECOMPRESS_RANDOM,
            Some(OUTPUT_BE_G2_DECOMPRESS_RANDOM),
            INPUT_LE_G2_DECOMPRESS_RANDOM,
            Some(OUTPUT_LE_G2_DECOMPRESS_RANDOM),
        );
    }

    #[test]
    fn test_g2_decompress_infinity() {
        run_g2_test(
            "INFINITY",
            INPUT_BE_G2_DECOMPRESS_INFINITY,
            Some(OUTPUT_BE_G2_DECOMPRESS_INFINITY),
            INPUT_LE_G2_DECOMPRESS_INFINITY,
            Some(OUTPUT_LE_G2_DECOMPRESS_INFINITY),
        );
    }

    #[test]
    fn test_g2_decompress_generator() {
        run_g2_test(
            "GENERATOR",
            INPUT_BE_G2_DECOMPRESS_GENERATOR,
            Some(OUTPUT_BE_G2_DECOMPRESS_GENERATOR),
            INPUT_LE_G2_DECOMPRESS_GENERATOR,
            Some(OUTPUT_LE_G2_DECOMPRESS_GENERATOR),
        );
    }

    #[test]
    fn test_g2_decompress_invalid() {
        run_g2_test(
            "INVALID_CURVE",
            INPUT_BE_G2_DECOMPRESS_RANDOM_INVALID_CURVE,
            None,
            INPUT_LE_G2_DECOMPRESS_RANDOM_INVALID_CURVE,
            None,
        );
        run_g2_test(
            "INVALID_FIELD",
            INPUT_BE_G2_DECOMPRESS_FIELD_TOO_LARGE_INVALID,
            None,
            INPUT_LE_G2_DECOMPRESS_FIELD_TOO_LARGE_INVALID,
            None,
        );
    }

    #[test]
    fn test_invalid_length() {
        assert!(bls12_381_g1_decompress(Version::V0, &[0u8; 47], Endianness::BE).is_none());
        assert!(bls12_381_g2_decompress(Version::V0, &[0u8; 95], Endianness::BE).is_none());
    }
}
