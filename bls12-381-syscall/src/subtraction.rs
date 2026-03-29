use {
    crate::{
        Version,
        encoding::{Endianness, PodG1Point, PodG2Point, swap_fq_endianness, swap_g2_c0_c1},
    },
    blstrs::{G1Projective, G2Projective},
    group::prime::PrimeCurveAffine,
};

/// Performs point subtraction on G1: `P1 - P2`.
pub fn bls12_381_g1_subtraction(
    _version: Version,
    p1: &PodG1Point,
    p2: &PodG1Point,
    endianness: Endianness,
) -> Option<PodG1Point> {
    // skip subgroup check for efficiency
    let p1_affine = p1.to_affine_subgroup_unchecked(endianness)?;
    let p2_affine = p2.to_affine_subgroup_unchecked(endianness)?;

    let diff_affine = if bool::from(p1_affine.is_identity()) {
        #[allow(clippy::arithmetic_side_effects)]
        (-p2_affine).to_uncompressed()
    } else {
        #[allow(clippy::arithmetic_side_effects)]
        (G1Projective::from(p1_affine) - p2_affine).to_uncompressed()
    };

    let mut result = PodG1Point(diff_affine);
    if matches!(endianness, Endianness::LE) {
        swap_fq_endianness(&mut result.0);
    }
    Some(result)
}

/// Performs point subtraction on G2: `P1 - P2`.
pub fn bls12_381_g2_subtraction(
    _version: Version,
    p1: &PodG2Point,
    p2: &PodG2Point,
    endianness: Endianness,
) -> Option<PodG2Point> {
    // skip subgroup check for efficiency
    let p1_affine = p1.to_affine_subgroup_unchecked(endianness)?;
    let p2_affine = p2.to_affine_subgroup_unchecked(endianness)?;

    let diff_affine = if bool::from(p1_affine.is_identity()) {
        #[allow(clippy::arithmetic_side_effects)]
        (-p2_affine).to_uncompressed()
    } else {
        #[allow(clippy::arithmetic_side_effects)]
        (G2Projective::from(p1_affine) - p2_affine).to_uncompressed()
    };

    let mut result = PodG2Point(diff_affine);
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

    fn run_g1_test(
        test_name: &str,
        input_be: &[u8],
        output_be: &[u8],
        input_le: &[u8],
        output_le: &[u8],
    ) {
        let (p1_be, p2_be) = input_be.split_at(96);
        let p1_be = to_pod_g1(p1_be);
        let p2_be = to_pod_g1(p2_be);
        let expected_be = to_pod_g1(output_be);

        let result_be = bls12_381_g1_subtraction(Version::V0, &p1_be, &p2_be, Endianness::BE);
        assert_eq!(
            result_be,
            Some(expected_be),
            "G1 {test_name} BE Test Failed",
        );

        let (p1_le, p2_le) = input_le.split_at(96);
        let p1_le = to_pod_g1(p1_le);
        let p2_le = to_pod_g1(p2_le);
        let expected_le = to_pod_g1(output_le);

        let result_le = bls12_381_g1_subtraction(Version::V0, &p1_le, &p2_le, Endianness::LE);
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
        let (p1_be, p2_be) = input_be.split_at(192);
        let p1_be = to_pod_g2(p1_be);
        let p2_be = to_pod_g2(p2_be);
        let expected_be = to_pod_g2(output_be);

        let result_be = bls12_381_g2_subtraction(Version::V0, &p1_be, &p2_be, Endianness::BE);
        assert_eq!(
            result_be,
            Some(expected_be),
            "G2 {test_name} BE Test Failed",
        );

        let (p1_le, p2_le) = input_le.split_at(192);
        let p1_le = to_pod_g2(p1_le);
        let p2_le = to_pod_g2(p2_le);
        let expected_le = to_pod_g2(output_le);

        let result_le = bls12_381_g2_subtraction(Version::V0, &p1_le, &p2_le, Endianness::LE);
        assert_eq!(
            result_le,
            Some(expected_le),
            "G2 {test_name} LE Test Failed",
        );
    }

    #[test]
    fn test_g1_subtraction_random() {
        run_g1_test(
            "SUB: P (Rand) - Q (Rand)",
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
            "SUB: P - P",
            INPUT_BE_G1_SUB_P_MINUS_P,
            OUTPUT_BE_G1_SUB_P_MINUS_P,
            INPUT_LE_G1_SUB_P_MINUS_P,
            OUTPUT_LE_G1_SUB_P_MINUS_P,
        );
    }

    #[test]
    fn test_g1_subtraction_infinity_edge_cases() {
        // Inf - P (Should result in -P)
        run_g1_test(
            "SUB: Inf - P",
            INPUT_BE_G1_SUB_INF_MINUS_P,
            OUTPUT_BE_G1_SUB_INF_MINUS_P,
            INPUT_LE_G1_SUB_INF_MINUS_P,
            OUTPUT_LE_G1_SUB_INF_MINUS_P,
        );

        // P - Inf (Should result in P)
        run_g1_test(
            "SUB: P - Inf",
            INPUT_BE_G1_SUB_P_MINUS_INF,
            OUTPUT_BE_G1_SUB_P_MINUS_INF,
            INPUT_LE_G1_SUB_P_MINUS_INF,
            OUTPUT_LE_G1_SUB_P_MINUS_INF,
        );
    }

    #[test]
    fn test_g2_subtraction_random() {
        run_g2_test(
            "SUB: P (Rand) - Q (Rand)",
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
            "SUB: P - P",
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
            "SUB: Inf - P",
            INPUT_BE_G2_SUB_INF_MINUS_P,
            OUTPUT_BE_G2_SUB_INF_MINUS_P,
            INPUT_LE_G2_SUB_INF_MINUS_P,
            OUTPUT_LE_G2_SUB_INF_MINUS_P,
        );

        // P - Inf (Should result in P)
        run_g2_test(
            "SUB: P - Inf",
            INPUT_BE_G2_SUB_P_MINUS_INF,
            OUTPUT_BE_G2_SUB_P_MINUS_INF,
            INPUT_LE_G2_SUB_P_MINUS_INF,
            OUTPUT_LE_G2_SUB_P_MINUS_INF,
        );
    }
}
