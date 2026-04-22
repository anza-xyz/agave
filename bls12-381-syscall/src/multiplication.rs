use {
    crate::{
        Version,
        encoding::{
            Endianness, PodG1Point, PodG2Point, PodScalar, swap_fq_endianness, swap_g2_c0_c1,
        },
    },
    blstrs::{G1Projective, G2Projective},
};

/// Performs scalar multiplication on G1: `P * s`.
pub fn bls12_381_g1_multiplication(
    _version: Version,
    point: &PodG1Point,
    scalar: &PodScalar,
    endianness: Endianness,
) -> Option<PodG1Point> {
    // perform full validation of points
    let p1_affine = point.to_affine(endianness)?;
    let scalar_val = scalar.to_scalar(endianness)?;

    #[allow(clippy::arithmetic_side_effects)]
    let result_proj = G1Projective::from(p1_affine) * scalar_val;
    let result_affine = result_proj.to_uncompressed();

    let mut result = PodG1Point(result_affine);
    if matches!(endianness, Endianness::LE) {
        swap_fq_endianness(&mut result.0);
    }
    Some(result)
}

/// Performs scalar multiplication on G2: `P * s`.
pub fn bls12_381_g2_multiplication(
    _version: Version,
    point: &PodG2Point,
    scalar: &PodScalar,
    endianness: Endianness,
) -> Option<PodG2Point> {
    // perform full validation of points
    let p1_affine = point.to_affine(endianness)?;
    let scalar_val = scalar.to_scalar(endianness)?;

    #[allow(clippy::arithmetic_side_effects)]
    let result_proj = G2Projective::from(p1_affine) * scalar_val;
    let result_affine = result_proj.to_uncompressed();

    let mut result = PodG2Point(result_affine);
    if matches!(endianness, Endianness::LE) {
        swap_g2_c0_c1(&mut result.0);
        swap_fq_endianness(&mut result.0);
    }
    Some(result)
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

    fn to_pod_scalar(bytes: &[u8]) -> PodScalar {
        pod_read_unaligned(bytes)
    }

    fn run_g1_test(
        test_name: &str,
        input_be: &[u8],
        output_be: &[u8],
        input_le: &[u8],
        output_le: &[u8],
    ) {
        // G1 Input is [Point (96) | Scalar (32)]
        let (point_be, scalar_be) = input_be.split_at(96);
        let point_be = to_pod_g1(point_be);
        let scalar_be = to_pod_scalar(scalar_be);
        let expected_be = to_pod_g1(output_be);

        let result_be =
            bls12_381_g1_multiplication(Version::V0, &point_be, &scalar_be, Endianness::BE);
        assert_eq!(
            result_be,
            Some(expected_be),
            "G1 {test_name} BE Test Failed",
        );

        // G1 Input is [Point (96) | Scalar (32)]
        let (point_le, scalar_le) = input_le.split_at(96);
        let point_le = to_pod_g1(point_le);
        let scalar_le = to_pod_scalar(scalar_le);
        let expected_le = to_pod_g1(output_le);

        let result_le =
            bls12_381_g1_multiplication(Version::V0, &point_le, &scalar_le, Endianness::LE);
        assert_eq!(
            result_le,
            Some(expected_le),
            "G1 {test_name} LE Test Failed",
        );
    }

    fn run_g2_test(
        test_name: &str,
        input_be: &[u8],
        output_be: &[u8],
        input_le: &[u8],
        output_le: &[u8],
    ) {
        // G2 Input is [Point (192) | Scalar (32)]
        let (point_be, scalar_be) = input_be.split_at(192);
        let point_be = to_pod_g2(point_be);
        let scalar_be = to_pod_scalar(scalar_be);
        let expected_be = to_pod_g2(output_be);

        let result_be =
            bls12_381_g2_multiplication(Version::V0, &point_be, &scalar_be, Endianness::BE);
        assert_eq!(
            result_be,
            Some(expected_be),
            "G2 {test_name} BE Test Failed",
        );

        let (point_le, scalar_le) = input_le.split_at(192);
        let point_le = to_pod_g2(point_le);
        let scalar_le = to_pod_scalar(scalar_le);
        let expected_le = to_pod_g2(output_le);

        let result_le =
            bls12_381_g2_multiplication(Version::V0, &point_le, &scalar_le, Endianness::LE);
        assert_eq!(
            result_le,
            Some(expected_le),
            "G2 {test_name} LE Test Failed",
        );
    }

    #[test]
    fn test_g1_multiplication_random() {
        run_g1_test(
            "MUL: P * Scalar (Random)",
            INPUT_BE_G1_MUL_RANDOM,
            OUTPUT_BE_G1_MUL_RANDOM,
            INPUT_LE_G1_MUL_RANDOM,
            OUTPUT_LE_G1_MUL_RANDOM,
        );
    }

    #[test]
    fn test_g1_multiplication_zero() {
        run_g1_test(
            "MUL: P * 0",
            INPUT_BE_G1_MUL_SCALAR_ZERO,
            OUTPUT_BE_G1_MUL_SCALAR_ZERO,
            INPUT_LE_G1_MUL_SCALAR_ZERO,
            OUTPUT_LE_G1_MUL_SCALAR_ZERO,
        );
    }

    #[test]
    fn test_g1_multiplication_one() {
        run_g1_test(
            "MUL: P * 1",
            INPUT_BE_G1_MUL_SCALAR_ONE,
            OUTPUT_BE_G1_MUL_SCALAR_ONE,
            INPUT_LE_G1_MUL_SCALAR_ONE,
            OUTPUT_LE_G1_MUL_SCALAR_ONE,
        );
    }

    #[test]
    fn test_g1_multiplication_minus_one() {
        run_g1_test(
            "MUL: P * -1",
            INPUT_BE_G1_MUL_SCALAR_MINUS_ONE,
            OUTPUT_BE_G1_MUL_SCALAR_MINUS_ONE,
            INPUT_LE_G1_MUL_SCALAR_MINUS_ONE,
            OUTPUT_LE_G1_MUL_SCALAR_MINUS_ONE,
        );
    }

    #[test]
    fn test_g1_multiplication_infinity() {
        run_g1_test(
            "MUL: Infinity * Scalar",
            INPUT_BE_G1_MUL_POINT_INFINITY,
            OUTPUT_BE_G1_MUL_POINT_INFINITY,
            INPUT_LE_G1_MUL_POINT_INFINITY,
            OUTPUT_LE_G1_MUL_POINT_INFINITY,
        );
    }

    #[test]
    fn test_g2_multiplication_random() {
        run_g2_test(
            "MUL: P * Scalar (Random)",
            INPUT_BE_G2_MUL_RANDOM,
            OUTPUT_BE_G2_MUL_RANDOM,
            INPUT_LE_G2_MUL_RANDOM,
            OUTPUT_LE_G2_MUL_RANDOM,
        );
    }

    #[test]
    fn test_g2_multiplication_zero() {
        run_g2_test(
            "MUL: P * 0",
            INPUT_BE_G2_MUL_SCALAR_ZERO,
            OUTPUT_BE_G2_MUL_SCALAR_ZERO,
            INPUT_LE_G2_MUL_SCALAR_ZERO,
            OUTPUT_LE_G2_MUL_SCALAR_ZERO,
        );
    }

    #[test]
    fn test_g2_multiplication_one() {
        run_g2_test(
            "MUL: P * 1",
            INPUT_BE_G2_MUL_SCALAR_ONE,
            OUTPUT_BE_G2_MUL_SCALAR_ONE,
            INPUT_LE_G2_MUL_SCALAR_ONE,
            OUTPUT_LE_G2_MUL_SCALAR_ONE,
        );
    }

    #[test]
    fn test_g2_multiplication_minus_one() {
        run_g2_test(
            "MUL: P * -1",
            INPUT_BE_G2_MUL_SCALAR_MINUS_ONE,
            OUTPUT_BE_G2_MUL_SCALAR_MINUS_ONE,
            INPUT_LE_G2_MUL_SCALAR_MINUS_ONE,
            OUTPUT_LE_G2_MUL_SCALAR_MINUS_ONE,
        );
    }

    #[test]
    fn test_g2_multiplication_infinity() {
        run_g2_test(
            "MUL: Inf * Scalar",
            INPUT_BE_G2_MUL_POINT_INFINITY,
            OUTPUT_BE_G2_MUL_POINT_INFINITY,
            INPUT_LE_G2_MUL_POINT_INFINITY,
            OUTPUT_LE_G2_MUL_POINT_INFINITY,
        );
    }
}
