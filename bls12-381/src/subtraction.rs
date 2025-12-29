use {
    crate::{reverse_48_byte_chunks, swap_g2_c0_c1, Endianness, Version},
    blstrs::{G1Affine, G1Projective, G2Affine, G2Projective},
    group::prime::PrimeCurveAffine,
    std::convert::TryInto,
};

pub fn bls12_381_g1_subtraction(
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

    let mut diff_affine = if bool::from(p1.is_identity()) {
        #[allow(clippy::arithmetic_side_effects)]
        (-p2).to_uncompressed()
    } else {
        #[allow(clippy::arithmetic_side_effects)]
        (G1Projective::from(p1) - p2).to_uncompressed()
    };

    if matches!(endianness, Endianness::LE) {
        reverse_48_byte_chunks(&mut diff_affine);
    }
    Some(diff_affine.to_vec())
}

pub fn bls12_381_g2_subtraction(
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
            reverse_48_byte_chunks(&mut bytes);
            swap_g2_c0_c1(&mut bytes);
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

    let mut diff_affine = if bool::from(p1.is_identity()) {
        #[allow(clippy::arithmetic_side_effects)]
        (-p2).to_uncompressed()
    } else {
        #[allow(clippy::arithmetic_side_effects)]
        (G2Projective::from(p1) - p2).to_uncompressed()
    };

    if matches!(endianness, Endianness::LE) {
        swap_g2_c0_c1(&mut diff_affine);
        reverse_48_byte_chunks(&mut diff_affine);
    }
    Some(diff_affine.to_vec())
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
    fn test_g1_subtraction_random() {
        run_g1_test(
            "SUB",
            bls12_381_g1_subtraction,
            INPUT_BE_G1_SUB_RANDOM,
            OUTPUT_BE_G1_SUB_RANDOM,
            INPUT_LE_G1_SUB_RANDOM,
            OUTPUT_LE_G1_SUB_RANDOM,
        );
    }

    #[test]
    fn test_g1_subtraction_p_minus_p() {
        // Result should be Identity
        run_g1_test(
            "SUB",
            bls12_381_g1_subtraction,
            INPUT_BE_G1_SUB_P_MINUS_P,
            OUTPUT_BE_G1_SUB_P_MINUS_P,
            INPUT_LE_G1_SUB_P_MINUS_P,
            OUTPUT_LE_G1_SUB_P_MINUS_P,
        );
    }

    #[test]
    fn test_g1_subtraction_infinity_edge_cases() {
        // Inf - P (Should result in -P) - This verifies your recent fix
        run_g1_test(
            "SUB",
            bls12_381_g1_subtraction,
            INPUT_BE_G1_SUB_INF_MINUS_P,
            OUTPUT_BE_G1_SUB_INF_MINUS_P,
            INPUT_LE_G1_SUB_INF_MINUS_P,
            OUTPUT_LE_G1_SUB_INF_MINUS_P,
        );

        // P - Inf (Should result in P)
        run_g1_test(
            "SUB",
            bls12_381_g1_subtraction,
            INPUT_BE_G1_SUB_P_MINUS_INF,
            OUTPUT_BE_G1_SUB_P_MINUS_INF,
            INPUT_LE_G1_SUB_P_MINUS_INF,
            OUTPUT_LE_G1_SUB_P_MINUS_INF,
        );
    }

    #[test]
    fn test_g2_subtraction_random() {
        run_g2_test(
            "SUB",
            bls12_381_g2_subtraction,
            INPUT_BE_G2_SUB_RANDOM,
            OUTPUT_BE_G2_SUB_RANDOM,
            INPUT_LE_G2_SUB_RANDOM,
            OUTPUT_LE_G2_SUB_RANDOM,
        );
    }

    #[test]
    fn test_g2_subtraction_p_minus_p() {
        // Result should be Identity
        run_g2_test(
            "SUB",
            bls12_381_g2_subtraction,
            INPUT_BE_G2_SUB_P_MINUS_P,
            OUTPUT_BE_G2_SUB_P_MINUS_P,
            INPUT_LE_G2_SUB_P_MINUS_P,
            OUTPUT_LE_G2_SUB_P_MINUS_P,
        );
    }

    #[test]
    fn test_g2_subtraction_infinity_edge_cases() {
        // Inf - P (Should result in -P)
        run_g2_test(
            "SUB",
            bls12_381_g2_subtraction,
            INPUT_BE_G2_SUB_INF_MINUS_P,
            OUTPUT_BE_G2_SUB_INF_MINUS_P,
            INPUT_LE_G2_SUB_INF_MINUS_P,
            OUTPUT_LE_G2_SUB_INF_MINUS_P,
        );

        // P - Inf (Should result in P)
        run_g2_test(
            "SUB",
            bls12_381_g2_subtraction,
            INPUT_BE_G2_SUB_P_MINUS_INF,
            OUTPUT_BE_G2_SUB_P_MINUS_INF,
            INPUT_LE_G2_SUB_P_MINUS_INF,
            OUTPUT_LE_G2_SUB_P_MINUS_INF,
        );
    }

    #[test]
    fn test_invalid_length() {
        // G1 expects 192 bytes
        assert!(bls12_381_g1_subtraction(Version::V0, &[0u8; 191], Endianness::BE).is_none());
        // G2 expects 384 bytes
        assert!(bls12_381_g2_subtraction(Version::V0, &[0u8; 383], Endianness::BE).is_none());
    }
}
