use crate::{
    encoding::{Endianness, PodG1Point, PodG2Point},
    Version,
};

/// Validates that a G1 point is on the curve and in the correct subgroup.
pub fn bls12_381_g1_point_validation(
    _version: Version,
    input: &PodG1Point,
    endianness: Endianness,
) -> bool {
    // to_affine performs Field, On-Curve, and Subgroup checks
    input.to_affine(endianness).is_some()
}

/// Validates that a G2 point is on the curve and in the correct subgroup.
pub fn bls12_381_g2_point_validation(
    _version: Version,
    input: &PodG2Point,
    endianness: Endianness,
) -> bool {
    // to_affine performs Field, On-Curve, and Subgroup checks
    input.to_affine(endianness).is_some()
}

#[cfg(test)]
mod tests {
    use {super::*, crate::test_vectors::*, bytemuck::pod_read_unaligned};

    fn to_pod_g1(bytes: &[u8]) -> PodG1Point {
        pod_read_unaligned(bytes)
    }

    fn to_pod_g2(bytes: &[u8]) -> PodG2Point {
        pod_read_unaligned(bytes)
    }

    fn run_g1_test(op_name: &str, input_be: &[u8], expected_valid: bool, input_le: &[u8]) {
        let input_be_pod = to_pod_g1(input_be);
        let result_be = bls12_381_g1_point_validation(Version::V0, &input_be_pod, Endianness::BE);
        assert_eq!(
            result_be, expected_valid,
            "G1 {op_name} BE Validation Failed. Expected {expected_valid}, got {result_be}",
        );

        let input_le_pod = to_pod_g1(input_le);
        let result_le = bls12_381_g1_point_validation(Version::V0, &input_le_pod, Endianness::LE);
        assert_eq!(
            result_le, expected_valid,
            "G1 {op_name} LE Validation Failed. Expected {expected_valid}, got {result_le}",
        );
    }

    fn run_g2_test(op_name: &str, input_be: &[u8], expected_valid: bool, input_le: &[u8]) {
        let input_be_pod = to_pod_g2(input_be);
        let result_be = bls12_381_g2_point_validation(Version::V0, &input_be_pod, Endianness::BE);
        assert_eq!(
            result_be, expected_valid,
            "G2 {op_name} BE Validation Failed. Expected {expected_valid}, got {result_be}",
        );

        let input_le_pod = to_pod_g2(input_le);
        let result_le = bls12_381_g2_point_validation(Version::V0, &input_le_pod, Endianness::LE);
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
}
