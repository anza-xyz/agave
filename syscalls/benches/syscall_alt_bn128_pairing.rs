//! `sol_alt_bn128_group_op`, pairing variants.
//!
//! Unlike the flat-priced group ops, pairing has a two-term model:
//!
//!   one_pair_cost_first
//!     + one_pair_cost_other * (n - 1)
//!     + sha256_base_cost + input_size + ALT_BN128_PAIRING_OUTPUT_SIZE
//!
//! The structural claim is that the marginal pair costs less than the first,
//! because the final exponentiation runs once while the Miller loop runs per
//! pair. This file measures whether `one_pair_cost_other` matches that slope.

#[macro_use]
mod common;

use {
    ark_bn254::{Fr, G1Affine, G1Projective, G2Affine},
    ark_ec::{CurveGroup, AffineRepr},
    common::{bn254::*, *},
    criterion::{criterion_group, criterion_main, BenchmarkId},
    solana_bn254::versioned::{
        alt_bn128_versioned_pairing, VersionedPairing, ALT_BN128_PAIRING_BE,
        ALT_BN128_PAIRING_LE, ALT_BN128_PAIRING_OUTPUT_SIZE,
    },
    solana_syscalls::SyscallAltBn128,
    std::hint::black_box,
};

const INPUT_VA: u64 = va(0);
const RESULT_VA: u64 = va(1);

/// 4 is a Groth16 verification. 112 is roughly what fits in a 1.4M CU
/// transaction, so it bounds what the price has to cover.
const PAIR_COUNTS: &[usize] = &[1, 2, 4, 8, 16, 32, 64, 112];

/// `n` distinct non-identity pairs. Distinct points so no implementation can
/// dedupe, non-identity so the Miller loop never short-circuits.
fn pairing_input(n: usize, le: bool) -> Vec<u8> {
    let g1 = G1Affine::generator();
    let g2 = G2Affine::generator();
    let mut out = Vec::new();
    for i in 0..n {
        let p = (G1Projective::from(g1) * Fr::from(i as u64 + 1)).into_affine();
        out.extend_from_slice(&g1_bytes(&p, le));
        out.extend_from_slice(&g2_bytes(&g2, le));
    }
    out
}

fn bench_pairing(c: &mut Criterion) {
    for le in [false, true] {
        let tag = if le { "le" } else { "be" };
        let group_op = if le {
            ALT_BN128_PAIRING_LE
        } else {
            ALT_BN128_PAIRING_BE
        };

        // ---- Layer A
        let mut group = c.benchmark_group(format!("alt_bn128_pairing_{tag}"));
        configure(&mut group);
        for &n in PAIR_COUNTS {
            let input = pairing_input(n, le);
            group.bench_with_input(BenchmarkId::new("primitive", n), &n, |b, _| {
                b.iter(|| {
                    black_box(
                        alt_bn128_versioned_pairing(
                            VersionedPairing::V1,
                            black_box(input.as_slice()),
                            endian(le),
                        )
                        .unwrap(),
                    )
                })
            });
        }
        group.finish();

        // ---- Layer B
        for &n in PAIR_COUNTS {
            let input = pairing_input(n, le);
            let input_len = input.len() as u64;
            let mut result = [0u8; ALT_BN128_PAIRING_OUTPUT_SIZE];
            let config = Config::default();

            prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
            let memory_mapping = unsafe {
                MemoryMapping::new(
                    vec![
                        MemoryRegion::new(bytes_of_slice(input.as_slice()), INPUT_VA),
                        MemoryRegion::new(bytes_of_mut(&mut result), RESULT_VA),
                    ],
                    &config,
                    SBPFVersion::V3,
                )
                .unwrap()
            };
            invoke_context
                .memory_contexts
                .mock_set_mapping_abi_v1(memory_mapping);

            let cu = charged_cu!(
                invoke_context,
                SyscallAltBn128::rust(
                    &mut invoke_context,
                    group_op,
                    INPUT_VA,
                    input_len,
                    RESULT_VA,
                    0
                )
            );
            eprintln!("alt_bn128 pairing_{tag} n={n} -> {cu} CU");

            invoke_context.compute_meter.mock_set_remaining(u64::MAX);

            let mut group = c.benchmark_group(format!("alt_bn128_pairing_{tag}"));
            configure(&mut group);
            group.throughput(Throughput::Elements(cu));
            group.bench_with_input(BenchmarkId::new("syscall", n), &n, |b, _| {
                b.iter(|| {
                    black_box(
                        SyscallAltBn128::rust(
                            &mut invoke_context,
                            black_box(group_op),
                            black_box(INPUT_VA),
                            black_box(input_len),
                            black_box(RESULT_VA),
                            0,
                        )
                        .unwrap(),
                    )
                })
            });
            group.finish();
        }
    }
}

criterion_group!(benches, bench_pairing);
criterion_main!(benches);
