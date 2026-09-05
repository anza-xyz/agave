//! Pilot bench: `sol_secp256k1_recover`.
//!
//! Flat-priced via `secp256k1_recover_cost`, one call, no generics. The point
//! of this file is to prove the harness works, not to produce a result.

#[macro_use]
mod common;

use {
    common::*,
    criterion::{criterion_group, criterion_main},
    solana_syscalls::SyscallSecp256k1Recover,
    std::hint::black_box,
};

const HASH_VA: u64 = va(0);
const SIGNATURE_VA: u64 = va(1);
const RESULT_VA: u64 = va(2);

/// Build a signature that `libsecp256k1::recover` accepts, without needing the
/// `hmac` feature that signing requires.
///
/// Recovery does identical work whether or not the signature came from a real
/// key: parse `r` and `v` into a curve point `R`, then compute `r⁻¹(sR - eG)`.
/// About half of all candidate `r` values are valid x-coordinates, so this
/// lands within a couple of iterations.
fn valid_vector() -> ([u8; 32], [u8; 64], u64) {
    let message_hash = [0x11u8; 32];
    let message = libsecp256k1::Message::parse(&message_hash);
    let recovery_id = libsecp256k1::RecoveryId::parse(0).unwrap();

    for seed in 0u32..1024 {
        let mut sig_bytes = [0u8; 64];
        sig_bytes[..28].fill(0x11);
        sig_bytes[28..32].copy_from_slice(&seed.to_be_bytes());
        sig_bytes[32..].fill(0x22);

        let Ok(signature) = libsecp256k1::Signature::parse_standard_slice(&sig_bytes) else {
            continue;
        };
        if libsecp256k1::recover(&message, &signature, &recovery_id).is_ok() {
            return (message_hash, sig_bytes, 0);
        }
    }
    panic!("no recoverable signature found in 1024 candidates");
}

/// Layer A: the raw primitive, with no VM, no translation, no metering.
fn bench_primitive(c: &mut Criterion) {
    let (hash, signature, recovery_id) = valid_vector();
    let message = libsecp256k1::Message::parse(&hash);
    let signature = libsecp256k1::Signature::parse_standard_slice(&signature).unwrap();
    let recovery_id = libsecp256k1::RecoveryId::parse(recovery_id as u8).unwrap();

    let mut group = c.benchmark_group("secp256k1_recover");
    configure(&mut group);
    group.bench_function("primitive", |b| {
        b.iter(|| {
            black_box(
                libsecp256k1::recover(
                    black_box(&message),
                    black_box(&signature),
                    black_box(&recovery_id),
                )
                .unwrap(),
            )
        })
    });
    group.finish();
}

/// Layer B: the full syscall entry point, including memory translation,
/// the compute meter, and the feature-gate checks.
fn bench_syscall(c: &mut Criterion) {
    let (hash, signature, recovery_id) = valid_vector();
    let mut result = [0u8; 64];

    let config = Config::default();
    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());

    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![
                MemoryRegion::new(bytes_of(&hash), HASH_VA),
                MemoryRegion::new(bytes_of(&signature), SIGNATURE_VA),
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
        SyscallSecp256k1Recover::rust(
            &mut invoke_context,
            HASH_VA,
            recovery_id,
            SIGNATURE_VA,
            RESULT_VA,
            0,
        )
    );
    eprintln!("secp256k1_recover charges {cu} CU");

    // Set the budget once, outside the loop. Resetting per iteration would
    // time the reset. u64::MAX cannot be drained by any realistic sample count.
    invoke_context.compute_meter.mock_set_remaining(u64::MAX);

    let mut group = c.benchmark_group("secp256k1_recover");
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_function("syscall", |b| {
        b.iter(|| {
            black_box(
                SyscallSecp256k1Recover::rust(
                    black_box(&mut invoke_context),
                    HASH_VA,
                    recovery_id,
                    SIGNATURE_VA,
                    RESULT_VA,
                    0,
                )
                .unwrap(),
            )
        })
    });
    group.finish();
}

criterion_group!(benches, bench_primitive, bench_syscall);
criterion_main!(benches);
