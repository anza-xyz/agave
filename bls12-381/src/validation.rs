use {
    crate::{
        encoding::{reverse_48_byte_chunks, swap_g2_c0_c1, Endianness},
        Version,
    },
    blstrs::{G1Affine, G2Affine},
    std::convert::TryInto,
};

pub fn bls12_381_g1_point_validation(
    _version: Version,
    input: &[u8],
    endianness: Endianness,
) -> bool {
    if input.len() != 96 {
        return false;
    }

    let p1_opt = match endianness {
        Endianness::BE => {
            // ZERO-COPY: Cast slice directly to array reference
            let bytes: &[u8; 96] = input.try_into().expect("length checked");
            // from_uncompressed performs Field, On-Curve, and Subgroup checks
            G1Affine::from_uncompressed(bytes).into_option()
        }
        Endianness::LE => {
            // COPY & MUTATE: Allocate stack array to reverse bytes
            let mut bytes: [u8; 96] = input.try_into().expect("length checked");
            reverse_48_byte_chunks(&mut bytes);
            G1Affine::from_uncompressed(&bytes).into_option()
        }
    };

    p1_opt.is_some()
}

pub fn bls12_381_g2_point_validation(
    _version: Version,
    input: &[u8],
    endianness: Endianness,
) -> bool {
    if input.len() != 192 {
        return false;
    }

    let p2_opt = match endianness {
        Endianness::BE => {
            let bytes: &[u8; 192] = input.try_into().expect("length checked");
            G2Affine::from_uncompressed(bytes).into_option()
        }
        Endianness::LE => {
            let mut bytes: [u8; 192] = input.try_into().expect("length checked");
            // Apply G2 Little-Endian transformation
            reverse_48_byte_chunks(&mut bytes);
            swap_g2_c0_c1(&mut bytes);
            G2Affine::from_uncompressed(&bytes).into_option()
        }
    };

    p2_opt.is_some()
}

#[cfg(test)]
mod tests {
    use {super::*, crate::test_vectors::*};

    fn run_g1_test(op_name: &str, input_be: &[u8], expected_valid: bool, input_le: &[u8]) {
        // Test Big Endian
        let result_be = bls12_381_g1_point_validation(Version::V0, input_be, Endianness::BE);
        assert_eq!(
            result_be, expected_valid,
            "G1 {op_name} BE Validation Failed. Expected {expected_valid}, got {result_be}",
        );

        // Test Little Endian
        let result_le = bls12_381_g1_point_validation(Version::V0, input_le, Endianness::LE);
        assert_eq!(
            result_le, expected_valid,
            "G1 {op_name} LE Validation Failed. Expected {expected_valid}, got {result_le}",
        );
    }

    fn run_g2_test(op_name: &str, input_be: &[u8], expected_valid: bool, input_le: &[u8]) {
        // Test Big Endian
        let result_be = bls12_381_g2_point_validation(Version::V0, input_be, Endianness::BE);
        assert_eq!(
            result_be, expected_valid,
            "G2 {op_name} BE Validation Failed. Expected {expected_valid}, got {result_be}",
        );

        // Test Little Endian
        let result_le = bls12_381_g2_point_validation(Version::V0, input_le, Endianness::LE);
        assert_eq!(
            result_le, expected_valid,
            "G2 {op_name} LE Validation Failed. Expected {expected_valid}, got {result_le}",
        );
    }

    #[test]
    fn test_g1_validation_valid_points() {
        run_g1_test(
            "RANDOM",
            INPUT_BE_G1_VALIDATE_RANDOM_VALID,
            EXPECTED_G1_VALIDATE_RANDOM_VALID,
            INPUT_LE_G1_VALIDATE_RANDOM_VALID,
        );
        run_g1_test(
            "INFINITY",
            INPUT_BE_G1_VALIDATE_INFINITY_VALID,
            EXPECTED_G1_VALIDATE_INFINITY_VALID,
            INPUT_LE_G1_VALIDATE_INFINITY_VALID,
        );
        run_g1_test(
            "GENERATOR",
            INPUT_BE_G1_VALIDATE_GENERATOR_VALID,
            EXPECTED_G1_VALIDATE_GENERATOR_VALID,
            INPUT_LE_G1_VALIDATE_GENERATOR_VALID,
        );
    }

    #[test]
    fn test_g1_validation_invalid_points() {
        run_g1_test(
            "NOT_ON_CURVE",
            INPUT_BE_G1_VALIDATE_NOT_ON_CURVE_INVALID,
            EXPECTED_G1_VALIDATE_NOT_ON_CURVE_INVALID,
            INPUT_LE_G1_VALIDATE_NOT_ON_CURVE_INVALID,
        );
        run_g1_test(
            "FIELD_X_EQ_P",
            INPUT_BE_G1_VALIDATE_FIELD_X_EQ_P_INVALID,
            EXPECTED_G1_VALIDATE_FIELD_X_EQ_P_INVALID,
            INPUT_LE_G1_VALIDATE_FIELD_X_EQ_P_INVALID,
        );
    }

    #[test]
    fn test_g2_validation_valid_points() {
        run_g2_test(
            "RANDOM",
            INPUT_BE_G2_VALIDATE_RANDOM_VALID,
            EXPECTED_G2_VALIDATE_RANDOM_VALID,
            INPUT_LE_G2_VALIDATE_RANDOM_VALID,
        );
        run_g2_test(
            "INFINITY",
            INPUT_BE_G2_VALIDATE_INFINITY_VALID,
            EXPECTED_G2_VALIDATE_INFINITY_VALID,
            INPUT_LE_G2_VALIDATE_INFINITY_VALID,
        );
    }

    #[test]
    fn test_g2_validation_invalid_points() {
        run_g2_test(
            "NOT_ON_CURVE",
            INPUT_BE_G2_VALIDATE_NOT_ON_CURVE_INVALID,
            EXPECTED_G2_VALIDATE_NOT_ON_CURVE_INVALID,
            INPUT_LE_G2_VALIDATE_NOT_ON_CURVE_INVALID,
        );
        run_g2_test(
            "FIELD_X_EQ_P",
            INPUT_BE_G2_VALIDATE_FIELD_X_EQ_P_INVALID,
            EXPECTED_G2_VALIDATE_FIELD_X_EQ_P_INVALID,
            INPUT_LE_G2_VALIDATE_FIELD_X_EQ_P_INVALID,
        );
    }

    #[test]
    fn test_invalid_length() {
        assert!(!bls12_381_g1_point_validation(
            Version::V0,
            &[0u8; 95],
            Endianness::BE
        ),);
        assert!(!bls12_381_g2_point_validation(
            Version::V0,
            &[0u8; 191],
            Endianness::BE
        ),);
    }
}
