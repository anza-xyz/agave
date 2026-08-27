//! `sol_curve_pairing_map` for BLS12-381.
//!
//! Cost model:
//!   bls12_381_one_pair_cost + bls12_381_additional_pair_cost * (n - 1)
//!
//! Structurally, a multi-Miller loop runs once per pair while the final
//! exponentiation runs once for the whole batch, so the marginal pair should be
//! materially cheaper than the first. Whether `additional_pair_cost` reflects
//! that ratio is the question this file answers.
//!
//! SIMD-0388 bounds `num_pairs` to 8, so every value in 0..=8 is measured
//! rather than sampled. n=0 is included deliberately: the cost formula uses
//! `saturating_sub(1)`, so a zero-pair call is still charged the full
//! `one_pair_cost` while doing almost no work. It is the cleanest estimate of
//! pure syscall overhead available for this call.
//!
//! n=1, 2, 4 and 8 match the curve crate's own `benches/bench_main.rs`, so the
//! `primitive` numbers here can be cross-checked against
//! `cargo bench -p solana-bls12-381-syscall`.

#[macro_use]
mod common;

use {
    common::{bls12_381::*, *},
    criterion::{criterion_group, criterion_main, BenchmarkId},
    solana_bls12_381_syscall::{bls12_381_pairing_map, Version},
    solana_define_syscall::curve_constants::{BLS12_381_BE, BLS12_381_LE},
    solana_syscalls::SyscallCurvePairingMap,
    std::hint::black_box,
};

const G1_VA: u64 = va(0);
const G2_VA: u64 = va(1);
const RESULT_VA: u64 = va(2);

/// Maximum `num_pairs` per SIMD-0388.
const MAX_PAIRS: usize = 8;

fn pair_counts() -> Vec<usize> {
    (0..=MAX_PAIRS).collect()
}

fn curve_id(le: bool) -> u64 {
    if le {
        BLS12_381_LE
    } else {
        BLS12_381_BE
    }
}

fn tag(le: bool) -> &'static str {
    if le {
        "le"
    } else {
        "be"
    }
}

// ---------------------------------------------------------------- layer A

/// The pairing map itself: no VM, no translation, no metering.
fn bench_primitive(c: &mut Criterion, le: bool) {
    let mut group = c.benchmark_group(format!("bls12_381_pairing_{}", tag(le)));
    configure(&mut group);

    for n in pair_counts() {
        // Always allocate at least one pair so the slice has a valid backing
        // allocation, then take a logical view of length `n`.
        let (g1, g2) = pairing_batch(n.max(1), le);
        assert!(
            bls12_381_pairing_map(Version::V0, &g1[..n], &g2[..n], endianness(le)).is_some(),
            "pairing map failed at n={n} (le={le})"
        );

        group.bench_with_input(BenchmarkId::new("primitive", n), &n, |b, _| {
            b.iter(|| {
                black_box(bls12_381_pairing_map(
                    Version::V0,
                    black_box(&g1[..n]),
                    black_box(&g2[..n]),
                    endianness(le),
                ))
            })
        });
    }
    group.finish();
}

// ---------------------------------------------------------------- layer B

/// The full syscall entry point, including memory translation and metering.
fn bench_syscall(c: &mut Criterion, le: bool) {
    let id = curve_id(le);

    for n in pair_counts() {
        let (g1, g2) = pairing_batch(n.max(1), le);
        let mut result = vec![0u8; GT_SIZE];
        let config = Config::default();

        prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
        let memory_mapping = unsafe {
            MemoryMapping::new(
                vec![
                    MemoryRegion::new(bytes_of_slice(g1.as_slice()), G1_VA),
                    MemoryRegion::new(bytes_of_slice(g2.as_slice()), G2_VA),
                    MemoryRegion::new(bytes_of_slice_mut(result.as_mut_slice()), RESULT_VA),
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
            SyscallCurvePairingMap::rust(
                &mut invoke_context,
                id,
                n as u64,
                G1_VA,
                G2_VA,
                RESULT_VA
            )
        );
        eprintln!("bls12_381 pairing_{} n={n} -> {cu} CU", tag(le));

        invoke_context.compute_meter.mock_set_remaining(u64::MAX);

        let mut group = c.benchmark_group(format!("bls12_381_pairing_{}", tag(le)));
        configure(&mut group);
        group.throughput(Throughput::Elements(cu));
        group.bench_with_input(BenchmarkId::new("syscall", n), &n, |b, _| {
            b.iter(|| {
                black_box(
                    SyscallCurvePairingMap::rust(
                        &mut invoke_context,
                        black_box(id),
                        black_box(n as u64),
                        black_box(G1_VA),
                        black_box(G2_VA),
                        black_box(RESULT_VA),
                    )
                    .unwrap(),
                )
            })
        });
        group.finish();
    }
}

// ---------------------------------------------------------------- conformance

/// Reports whether the implementation enforces the SIMD-0388 cap of 8.
///
/// Not a benchmark: it runs once and prints. Deliberately non-fatal, because
/// the answer is the point. If the cap is not enforced, that is a
/// spec/implementation divergence rather than a pricing issue, and other
/// clients that do enforce it would reject a transaction agave accepts.
fn probe_pair_cap(le: bool) {
    let over = MAX_PAIRS + 1;
    let (g1, g2) = pairing_batch(over, le);
    let mut result = vec![0u8; GT_SIZE];
    let config = Config::default();
    let id = curve_id(le);

    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![
                MemoryRegion::new(bytes_of_slice(g1.as_slice()), G1_VA),
                MemoryRegion::new(bytes_of_slice(g2.as_slice()), G2_VA),
                MemoryRegion::new(bytes_of_slice_mut(result.as_mut_slice()), RESULT_VA),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap()
    };
    invoke_context
        .memory_contexts
        .mock_set_mapping_abi_v1(memory_mapping);
    invoke_context.compute_meter.mock_set_remaining(u64::MAX);

    let outcome = SyscallCurvePairingMap::rust(
        &mut invoke_context,
        id,
        over as u64,
        G1_VA,
        G2_VA,
        RESULT_VA,
    );
    match outcome {
        Err(e) => eprintln!("pairing cap ENFORCED ({}): n={over} rejected: {e}", tag(le)),
        Ok(status) => eprintln!(
            "pairing cap NOT ENFORCED ({}): n={over} returned status {status}; \
             SIMD-0388 requires a maximum of {MAX_PAIRS}",
            tag(le)
        ),
    }
}

// ---------------------------------------------------------------- driver

fn bench_pairing(c: &mut Criterion) {
    for le in [false, true] {
        probe_pair_cap(le);
        bench_primitive(c, le);
        bench_syscall(c, le);
    }
}

criterion_group!(benches, bench_pairing);
criterion_main!(benches);
