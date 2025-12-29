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
    let mut buf = [0u8; 48];

    // Fp12 has two Fp6 coefficients (c0, c1)
    for fp6 in val.fp6.iter() {
        // Fp6 has three Fp2 coefficients (c0, c1, c2)
        for fp2 in fp6.fp2.iter() {
            // Fp2 has two Fp coefficients (c0, c1)
            // c0 is real, c1 is imaginary.
            let c0 = &fp2.fp[0];
            let c1 = &fp2.fp[1];

            // Apply Fp2 Ordering Rule
            // BE (Zcash): Imaginary (c1) first, then Real (c0)
            // LE: Real (c0) first, then Imaginary (c1)
            let fps = match endianness {
                Endianness::BE => [c1, c0],
                Endianness::LE => [c0, c1],
            };

            for fp in fps {
                unsafe {
                    match endianness {
                        Endianness::BE => blst_bendian_from_fp(buf.as_mut_ptr(), fp),
                        Endianness::LE => blst_lendian_from_fp(buf.as_mut_ptr(), fp),
                    }
                }
                out.extend_from_slice(&buf);
            }
        }
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
