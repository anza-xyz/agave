use {
    crate::{
        Version,
        encoding::{Endianness, PodG1Point, PodG2Point, swap_fq_endianness, swap_g2_c0_c1},
    },
    blstrs::{G1Projective, G2Projective},
};

/// Performs point addition on G1: `P1 + P2`.
pub fn bls12_381_g1_addition(
    _version: Version,
    p1: &PodG1Point,
    p2: &PodG1Point,
    endianness: Endianness,
) -> Option<PodG1Point> {
    // skip subgroup check for efficiency
    let p1_affine = p1.to_affine_subgroup_unchecked(endianness)?;
    let p2_affine = p2.to_affine_subgroup_unchecked(endianness)?;

    #[allow(clippy::arithmetic_side_effects)]
    let sum_proj = G1Projective::from(p1_affine) + p2_affine;

    let sum_affine = sum_proj.to_uncompressed();

    let mut result = PodG1Point(sum_affine);

    if matches!(endianness, Endianness::LE) {
        swap_fq_endianness(&mut result.0);
    }

    Some(result)
}

/// Performs point addition on G2: `P1 + P2`.
pub fn bls12_381_g2_addition(
    _version: Version,
    p1: &PodG2Point,
    p2: &PodG2Point,
    endianness: Endianness,
) -> Option<PodG2Point> {
    // skip subgroup check for efficiency
    let p1_affine = p1.to_affine_subgroup_unchecked(endianness)?;
    let p2_affine = p2.to_affine_subgroup_unchecked(endianness)?;

    #[allow(clippy::arithmetic_side_effects)]
    let sum_proj = G2Projective::from(p1_affine) + p2_affine;

    let sum_affine = sum_proj.to_uncompressed();

    let mut result = PodG2Point(sum_affine);

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
        // G1 Input is [P1 (96) | P2 (96)]
        let (p1_be, p2_be) = input_be.split_at(96);
        let p1_be = to_pod_g1(p1_be);
        let p2_be = to_pod_g1(p2_be);
        let expected_be = to_pod_g1(output_be);

        // Test Big Endian
        let result_be = bls12_381_g1_addition(Version::V0, &p1_be, &p2_be, Endianness::BE);
        assert_eq!(
            result_be,
            Some(expected_be),
            "G1 {test_name} BE Test Failed",
        );

        // Test Little Endian
        let (p1_le, p2_le) = input_le.split_at(96);
        let p1_le = to_pod_g1(p1_le);
        let p2_le = to_pod_g1(p2_le);
        let expected_le = to_pod_g1(output_le);

        let result_le = bls12_381_g1_addition(Version::V0, &p1_le, &p2_le, Endianness::LE);
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
        // G2 Input is [P1 (192) | P2 (192)]
        let (p1_be, p2_be) = input_be.split_at(192);
        let p1_be = to_pod_g2(p1_be);
        let p2_be = to_pod_g2(p2_be);
        let expected_be = to_pod_g2(output_be);

        // Test Big Endian
        let result_be = bls12_381_g2_addition(Version::V0, &p1_be, &p2_be, Endianness::BE);
        assert_eq!(
            result_be,
            Some(expected_be),
            "G2 {test_name} BE Test Failed",
        );

        // Test Little Endian
        let (p1_le, p2_le) = input_le.split_at(192);
        let p1_le = to_pod_g2(p1_le);
        let p2_le = to_pod_g2(p2_le);
        let expected_le = to_pod_g2(output_le);

        let result_le = bls12_381_g2_addition(Version::V0, &p1_le, &p2_le, Endianness::LE);
        assert_eq!(
            result_le,
            Some(expected_le),
            "G2 {test_name} LE Test Failed",
        );
    }

    #[test]
    fn test_g1_addition_random() {
        run_g1_test(
            "ADD: P (Rand) + Q (Rand)",
            INPUT_BE_G1_ADD_RANDOM,
            OUTPUT_BE_G1_ADD_RANDOM,
            INPUT_LE_G1_ADD_RANDOM,
            OUTPUT_LE_G1_ADD_RANDOM,
        );
    }

    #[test]
    fn test_g1_addition_doubling() {
        run_g1_test(
            "ADD: P + P (Doubling)",
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
            "ADD: P + Inf",
            INPUT_BE_G1_ADD_P_PLUS_INF,
            OUTPUT_BE_G1_ADD_P_PLUS_INF,
            INPUT_LE_G1_ADD_P_PLUS_INF,
            OUTPUT_LE_G1_ADD_P_PLUS_INF,
        );
        // Inf + Inf
        run_g1_test(
            "ADD: Inf + Inf",
            INPUT_BE_G1_ADD_INF_PLUS_INF,
            OUTPUT_BE_G1_ADD_INF_PLUS_INF,
            INPUT_LE_G1_ADD_INF_PLUS_INF,
            OUTPUT_LE_G1_ADD_INF_PLUS_INF,
        );
    }

    #[test]
    fn test_g2_addition_random() {
        run_g2_test(
            "ADD: P (Rand) + Q (Rand)",
            INPUT_BE_G2_ADD_RANDOM,
            OUTPUT_BE_G2_ADD_RANDOM,
            INPUT_LE_G2_ADD_RANDOM,
            OUTPUT_LE_G2_ADD_RANDOM,
        );
    }

    #[test]
    fn test_g2_addition_doubling() {
        run_g2_test(
            "ADD: P + P (Doubling)",
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
            "ADD: P + Inf",
            INPUT_BE_G2_ADD_P_PLUS_INF,
            OUTPUT_BE_G2_ADD_P_PLUS_INF,
            INPUT_LE_G2_ADD_P_PLUS_INF,
            OUTPUT_LE_G2_ADD_P_PLUS_INF,
        );
        // Inf + Inf
        run_g2_test(
            "ADD: Inf + Inf",
            INPUT_BE_G2_ADD_INF_PLUS_INF,
            OUTPUT_BE_G2_ADD_INF_PLUS_INF,
            INPUT_LE_G2_ADD_INF_PLUS_INF,
            OUTPUT_LE_G2_ADD_INF_PLUS_INF,
        );
    }
}
