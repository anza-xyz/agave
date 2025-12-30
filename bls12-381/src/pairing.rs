#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{Endianness, Version},
    blst::*,
    blstrs::{Bls12, G1Affine, G2Affine, G2Prepared, Gt},
    group::Group,
    pairing::{MillerLoopResult, MultiMillerLoop},
    std::convert::TryInto,
};

/// Helper to serialize Fp12 (Gt) according to SIMD Endianness rules.
/// Fp12 = c0(Fp6) + c1(Fp6)w.
/// Fp6 = c0(Fp2) + c1(Fp2)v + c2(Fp2)v^2.
/// Fp2 = c0(Fp) + c1(Fp)u.
/// SIMD/Zcash BE Rule for Fp2: c1 (imaginary) then c0 (real).
/// SIMD LE Rule for Fp2: c0 then c1.
fn serialize_gt(gt: Gt, endianness: Endianness) -> Vec<u8> {
    // blstrs::Gt is repr(transparent) over blst_fp12.
    // We transmute to access the internal coefficients directly because
    // blstrs does not expose the Fp12/Fp6/Fp2/Fp types publicly.
    let val: blst_fp12 = unsafe { std::mem::transmute(gt) };

    let mut out = Vec::with_capacity(576); // 12 * 48
    let mut ptr = out.as_mut_ptr();

    unsafe {
        for fp6 in val.fp6.iter() {
            for fp2 in fp6.fp2.iter() {
                let c0 = &fp2.fp[0];
                let c1 = &fp2.fp[1];

                let fps = match endianness {
                    Endianness::BE => [c1, c0],
                    Endianness::LE => [c0, c1],
                };

                for fp in fps {
                    match endianness {
                        Endianness::BE => blst_bendian_from_fp(ptr, fp),
                        Endianness::LE => blst_lendian_from_fp(ptr, fp),
                    }
                    ptr = ptr.add(48);
                }
            }
        }
        out.set_len(576);
    }
    out
}

pub fn bls12_381_pairing_map(
    _version: Version,
    num_pairs: u64,
    g1_bytes: &[u8],
    g2_bytes: &[u8],
    endianness: Endianness,
) -> Option<Vec<u8>> {
    let num_pairs = num_pairs as usize;

    // 1. Validation
    if num_pairs == 0 {
        return Some(serialize_gt(Gt::identity(), endianness));
    }

    // Strict buffer size check
    if g1_bytes.len() != num_pairs.checked_mul(96)? {
        return None;
    }
    if g2_bytes.len() != num_pairs.checked_mul(192)? {
        return None;
    }

    // 2. Parse Points
    // We collect them into vectors because multi_miller_loop requires a slice of references.
    let mut g1_points = Vec::with_capacity(num_pairs);
    let mut g2_points = Vec::with_capacity(num_pairs);

    for i in 0..num_pairs {
        // --- Parse G1 ---
        let start = i * 96;
        let chunk = &g1_bytes[start..start + 96];
        let p1 = match endianness {
            Endianness::BE => {
                let b: &[u8; 96] = chunk.try_into().unwrap();
                G1Affine::from_uncompressed(b).into_option()?
            }
            Endianness::LE => {
                let mut b: [u8; 96] = chunk.try_into().unwrap();
                crate::reverse_48_byte_chunks(&mut b);
                G1Affine::from_uncompressed(&b).into_option()?
            }
        };
        g1_points.push(p1);

        // --- Parse G2 ---
        let start = i * 192;
        let chunk = &g2_bytes[start..start + 192];
        let p2 = match endianness {
            Endianness::BE => {
                let b: &[u8; 192] = chunk.try_into().unwrap();
                G2Affine::from_uncompressed(b).into_option()?
            }
            Endianness::LE => {
                let mut b: [u8; 192] = chunk.try_into().unwrap();
                crate::reverse_48_byte_chunks(&mut b);
                crate::swap_g2_c0_c1(&mut b);
                G2Affine::from_uncompressed(&b).into_option()?
            }
        };
        g2_points.push(G2Prepared::from(p2));
    }

    // 3. Batch Pairing (Multi Miller Loop)
    // Create vector of references [(&G1, &G2Prepared)]
    let refs: Vec<(&G1Affine, &G2Prepared)> = g1_points.iter().zip(g2_points.iter()).collect();

    let miller_out = Bls12::multi_miller_loop(&refs);
    let gt = miller_out.final_exponentiation();

    // 4. Serialize Result
    Some(serialize_gt(gt, endianness))
}

#[cfg(test)]
mod tests {
    use {super::*, crate::test_vectors::*};

    fn run_pairing_test(
        op_name: &str,
        num_pairs: u64,
        input_be: &[u8],
        output_be: &[u8],
        input_le: &[u8],
        output_le: &[u8],
    ) {
        // Calculate split point for G1/G2 arrays
        // Input is [G1_1, ... G1_N, G2_1, ... G2_N]
        let g1_len = (num_pairs as usize) * 96;

        let (g1_be, g2_be) = input_be.split_at(g1_len);
        let result_be = bls12_381_pairing_map(Version::V0, num_pairs, g1_be, g2_be, Endianness::BE);
        assert_eq!(
            result_be,
            Some(output_be.to_vec()),
            "Pairing {} BE Test Failed",
            op_name
        );

        let (g1_le, g2_le) = input_le.split_at(g1_len);
        let result_le = bls12_381_pairing_map(Version::V0, num_pairs, g1_le, g2_le, Endianness::LE);
        assert_eq!(
            result_le,
            Some(output_le.to_vec()),
            "Pairing {} LE Test Failed",
            op_name
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

    #[test]
    fn test_pairing_invalid_length() {
        // Mismatched lengths
        let g1_bytes = [0u8; 96];
        let g2_bytes = [0u8; 191]; // Too short
        assert!(
            bls12_381_pairing_map(Version::V0, 1, &g1_bytes, &g2_bytes, Endianness::BE).is_none()
        );
    }
}
