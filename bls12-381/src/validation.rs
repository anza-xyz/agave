use crate::{
    Version,
    encoding::{Endianness, PodG1Point, PodG2Point},
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
    use {
        super::*,
        crate::{
            encoding::{swap_fq_endianness, swap_g2_c0_c1},
            test_vectors::*,
        },
        bytemuck::pod_read_unaligned,
    };

    fn to_pod_g1(bytes: &[u8]) -> PodG1Point {
        pod_read_unaligned(bytes)
    }

    fn to_pod_g2(bytes: &[u8]) -> PodG2Point {
        pod_read_unaligned(bytes)
    }

    fn run_g1_test(test_name: &str, input_be: &[u8], expected_valid: bool, input_le: &[u8]) {
        let input_be_pod = to_pod_g1(input_be);
        let result_be = bls12_381_g1_point_validation(Version::V0, &input_be_pod, Endianness::BE);
        assert_eq!(
            result_be, expected_valid,
            "G1 {test_name} BE Validation Failed. Expected {expected_valid}, got {result_be}",
        );

        if expected_valid {
            let point = input_be_pod.to_affine(Endianness::BE).unwrap();
            let bytes = point.to_uncompressed();
            assert_eq!(bytes, input_be_pod.0, "G1 {test_name} BE Round Trip Failed");
        }

        let input_le_pod = to_pod_g1(input_le);
        let result_le = bls12_381_g1_point_validation(Version::V0, &input_le_pod, Endianness::LE);
        assert_eq!(
            result_le, expected_valid,
            "G1 {test_name} LE Validation Failed. Expected {expected_valid}, got {result_le}",
        );

        if expected_valid {
            let point = input_le_pod.to_affine(Endianness::LE).unwrap();
            let mut bytes = point.to_uncompressed(); // Returns Zcash BE
            swap_fq_endianness(&mut bytes); // Convert to LE
            assert_eq!(bytes, input_le_pod.0, "G1 {test_name} LE Round Trip Failed");
        }
    }

    fn run_g2_test(test_name: &str, input_be: &[u8], expected_valid: bool, input_le: &[u8]) {
        let input_be_pod = to_pod_g2(input_be);
        let result_be = bls12_381_g2_point_validation(Version::V0, &input_be_pod, Endianness::BE);
        assert_eq!(
            result_be, expected_valid,
            "G2 {test_name} BE Validation Failed. Expected {expected_valid}, got {result_be}",
        );

        if expected_valid {
            let point = input_be_pod.to_affine(Endianness::BE).unwrap();
            let bytes = point.to_uncompressed();
            assert_eq!(bytes, input_be_pod.0, "G2 {test_name} BE Round Trip Failed");
        }

        let input_le_pod = to_pod_g2(input_le);
        let result_le = bls12_381_g2_point_validation(Version::V0, &input_le_pod, Endianness::LE);
        assert_eq!(
            result_le, expected_valid,
            "G2 {test_name} LE Validation Failed. Expected {expected_valid}, got {result_le}",
        );

        if expected_valid {
            let point = input_le_pod.to_affine(Endianness::LE).unwrap();
            let mut bytes = point.to_uncompressed(); // Returns Zcash BE (c1, c0)
            swap_fq_endianness(&mut bytes); // Convert elements to LE
            swap_g2_c0_c1(&mut bytes); // Convert to (c0, c1) layout
            assert_eq!(bytes, input_le_pod.0, "G2 {test_name} LE Round Trip Failed");
        }
    }

    #[test]
    fn test_g1_validation_valid_points() {
        run_g1_test(
            "Validate: RANDOM",
            INPUT_BE_G1_VALIDATE_RANDOM_VALID,
            EXPECTED_G1_VALIDATE_RANDOM_VALID,
            INPUT_LE_G1_VALIDATE_RANDOM_VALID,
        );
        run_g1_test(
            "Validate: INFINITY",
            INPUT_BE_G1_VALIDATE_INFINITY_VALID,
            EXPECTED_G1_VALIDATE_INFINITY_VALID,
            INPUT_LE_G1_VALIDATE_INFINITY_VALID,
        );
        run_g1_test(
            "Validate: GENERATOR",
            INPUT_BE_G1_VALIDATE_GENERATOR_VALID,
            EXPECTED_G1_VALIDATE_GENERATOR_VALID,
            INPUT_LE_G1_VALIDATE_GENERATOR_VALID,
        );
    }

    #[test]
    fn test_g1_validation_invalid_points() {
        run_g1_test(
            "Validate: NOT_ON_CURVE",
            INPUT_BE_G1_VALIDATE_NOT_ON_CURVE_INVALID,
            EXPECTED_G1_VALIDATE_NOT_ON_CURVE_INVALID,
            INPUT_LE_G1_VALIDATE_NOT_ON_CURVE_INVALID,
        );
        run_g1_test(
            "Validate: FIELD_X_EQ_P",
            INPUT_BE_G1_VALIDATE_FIELD_X_EQ_P_INVALID,
            EXPECTED_G1_VALIDATE_FIELD_X_EQ_P_INVALID,
            INPUT_LE_G1_VALIDATE_FIELD_X_EQ_P_INVALID,
        );
    }

    #[test]
    fn test_g2_validation_valid_points() {
        run_g2_test(
            "Validate: RANDOM",
            INPUT_BE_G2_VALIDATE_RANDOM_VALID,
            EXPECTED_G2_VALIDATE_RANDOM_VALID,
            INPUT_LE_G2_VALIDATE_RANDOM_VALID,
        );
        run_g2_test(
            "Validate: INFINITY",
            INPUT_BE_G2_VALIDATE_INFINITY_VALID,
            EXPECTED_G2_VALIDATE_INFINITY_VALID,
            INPUT_LE_G2_VALIDATE_INFINITY_VALID,
        );
    }

    #[test]
    fn test_g2_validation_invalid_points() {
        run_g2_test(
            "Validate: NOT_ON_CURVE",
            INPUT_BE_G2_VALIDATE_NOT_ON_CURVE_INVALID,
            EXPECTED_G2_VALIDATE_NOT_ON_CURVE_INVALID,
            INPUT_LE_G2_VALIDATE_NOT_ON_CURVE_INVALID,
        );
        run_g2_test(
            "Validate: FIELD_X_EQ_P",
            INPUT_BE_G2_VALIDATE_FIELD_X_EQ_P_INVALID,
            EXPECTED_G2_VALIDATE_FIELD_X_EQ_P_INVALID,
            INPUT_LE_G2_VALIDATE_FIELD_X_EQ_P_INVALID,
        );
    }
}
