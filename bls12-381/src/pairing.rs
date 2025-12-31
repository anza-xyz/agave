#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        encoding::{serialize_gt, Endianness, PodG1Point, PodG2Point, PodGtElement},
        Version,
    },
    blstrs::{Bls12, G1Affine, G2Prepared, Gt},
    group::Group,
    pairing::{MillerLoopResult, MultiMillerLoop},
};

/// Computes the product of pairings for a batch of G1 and G2 points.
///
/// Mathematically, this computes:
/// `e(P_1, Q_1) * e(P_2, Q_2) * ... * e(P_n, Q_n)`
pub fn bls12_381_pairing_map(
    _version: Version,
    g1_points: &[PodG1Point],
    g2_points: &[PodG2Point],
    endianness: Endianness,
) -> Option<PodGtElement> {
    if g1_points.len() != g2_points.len() {
        return None;
    }

    if g1_points.is_empty() {
        return Some(serialize_gt(Gt::identity(), endianness));
    }

    let count = g1_points.len();
    let mut g1_affines = Vec::with_capacity(count);
    let mut g2_prepareds = Vec::with_capacity(count);

    for (p1_pod, p2_pod) in g1_points.iter().zip(g2_points.iter()) {
        let p1 = p1_pod.to_affine(endianness)?;
        let p2 = p2_pod.to_affine(endianness)?;

        g1_affines.push(p1);
        g2_prepareds.push(G2Prepared::from(p2));
    }

    let refs: Vec<(&G1Affine, &G2Prepared)> = g1_affines.iter().zip(g2_prepareds.iter()).collect();

    let miller_out = Bls12::multi_miller_loop(&refs);
    let gt = miller_out.final_exponentiation();

    Some(serialize_gt(gt, endianness))
}

#[cfg(test)]
mod tests {
    use {super::*, crate::test_vectors::*, bytemuck::cast_slice};

    fn run_pairing_test(
        op_name: &str,
        num_pairs: u64,
        input_be: &[u8],
        output_be: &[u8],
        input_le: &[u8],
        output_le: &[u8],
    ) {
        let num_pairs = num_pairs as usize;
        let g1_len_bytes = num_pairs * 96;

        // --- Test Big Endian ---
        let (g1_bytes_be, g2_bytes_be) = input_be.split_at(g1_len_bytes);

        let g1_pods_be: &[PodG1Point] = cast_slice(g1_bytes_be);
        let g2_pods_be: &[PodG2Point] = cast_slice(g2_bytes_be);

        assert_eq!(g1_pods_be.len(), num_pairs);
        assert_eq!(g2_pods_be.len(), num_pairs);

        let result_be = bls12_381_pairing_map(Version::V0, g1_pods_be, g2_pods_be, Endianness::BE);
        let expected_be = PodGtElement(output_be.try_into().expect("valid output length"));

        assert_eq!(
            result_be,
            Some(expected_be),
            "Pairing {op_name} BE Test Failed",
        );

        // --- Test Little Endian ---
        let (g1_bytes_le, g2_bytes_le) = input_le.split_at(g1_len_bytes);
        let g1_pods_le: &[PodG1Point] = cast_slice(g1_bytes_le);
        let g2_pods_le: &[PodG2Point] = cast_slice(g2_bytes_le);

        let result_le = bls12_381_pairing_map(Version::V0, g1_pods_le, g2_pods_le, Endianness::LE);
        let expected_le = PodGtElement(output_le.try_into().expect("valid output length"));

        assert_eq!(
            result_le,
            Some(expected_le),
            "Pairing {op_name} LE Test Failed",
        );
    }

    #[test]
    fn test_pairing_identity() {
        run_pairing_test(
            "IDENTITY",
            0,
            INPUT_BE_PAIRING_IDENTITY,
            OUTPUT_BE_PAIRING_IDENTITY,
            INPUT_LE_PAIRING_IDENTITY,
            OUTPUT_LE_PAIRING_IDENTITY,
        );
    }

    #[test]
    fn test_pairing_one_pair() {
        run_pairing_test(
            "ONE_PAIR",
            1,
            INPUT_BE_PAIRING_ONE_PAIR,
            OUTPUT_BE_PAIRING_ONE_PAIR,
            INPUT_LE_PAIRING_ONE_PAIR,
            OUTPUT_LE_PAIRING_ONE_PAIR,
        );
    }

    #[test]
    fn test_pairing_two_pairs() {
        run_pairing_test(
            "TWO_PAIRS",
            2,
            INPUT_BE_PAIRING_TWO_PAIRS,
            OUTPUT_BE_PAIRING_TWO_PAIRS,
            INPUT_LE_PAIRING_TWO_PAIRS,
            OUTPUT_LE_PAIRING_TWO_PAIRS,
        );
    }

    #[test]
    fn test_pairing_three_pairs() {
        run_pairing_test(
            "THREE_PAIRS",
            3,
            INPUT_BE_PAIRING_THREE_PAIRS,
            OUTPUT_BE_PAIRING_THREE_PAIRS,
            INPUT_LE_PAIRING_THREE_PAIRS,
            OUTPUT_LE_PAIRING_THREE_PAIRS,
        );
    }

    #[test]
    fn test_pairing_bilinearity() {
        // e(aP, Q) * e(P, -aQ) == 1
        run_pairing_test(
            "BILINEARITY_IDENTITY",
            2,
            INPUT_BE_PAIRING_BILINEARITY_IDENTITY,
            OUTPUT_BE_PAIRING_BILINEARITY_IDENTITY,
            INPUT_LE_PAIRING_BILINEARITY_IDENTITY,
            OUTPUT_LE_PAIRING_BILINEARITY_IDENTITY,
        );
    }
}
