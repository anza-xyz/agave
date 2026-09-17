//! `sol_sha256`, `sol_keccak256`, `sol_blake3`, `sol_sha512`.
//!
//! All four go through the same generic `SyscallHash<H>` and, critically, all
//! four `HasherImpl` impls return `sha256_base_cost` / `sha256_byte_cost` /
//! `sha256_max_slices`. They are priced identically. This file measures
//! whether they cost the same.
//!
//! Charged CU for one call over `k` slices:
//!     sha256_base_cost + sum_i max(mem_op_base_cost, sha256_byte_cost * (len_i / 2))
//! Note the integer division by two: the "byte" cost is charged per 2 bytes.

#[macro_use]
mod common;

use {
    common::*,
    criterion::{criterion_group, criterion_main, BenchmarkId},
    solana_program_runtime::execution_budget::SVMTransactionExecutionBudget,
    solana_syscalls::{
        Blake3Hasher, HasherImpl, Keccak256Hasher, Sha256Hasher, Sha512Hasher, SyscallHash,
    },
    std::hint::black_box,
};

const VALS_VA: u64 = va(0);
const RESULT_VA: u64 = va(1);
const DATA_VA: u64 = va(2);

/// Result region is sized for the largest `H::Output` (Hash512 = 64 bytes).
const RESULT_LEN: usize = 64;

/// Per-slice byte lengths. Spans the region where the `mem_op_base_cost` floor
/// binds, through sizes a real program would hash, into the tail.
const LENGTHS: &[usize] = &[16, 32, 64, 128, 256, 1024, 4096, 16_384, 65_536];

/// Slice counts, each slice deliberately small so per-slice overhead dominates.
const COUNT_SWEEP_SLICE_LEN: usize = 16;

// ---------------------------------------------------------------- layer A

/// The raw hasher, no VM, no translation, no metering.
fn primitive_by_length<H: HasherImpl>(c: &mut Criterion, name: &str) {
    let mut group = c.benchmark_group(format!("hash_{name}"));
    configure(&mut group);
    for &len in LENGTHS {
        let data = vec![0xa5u8; len];
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::new("primitive", len), &len, |b, _| {
            b.iter(|| {
                let mut hasher = H::create_hasher();
                hasher.hash(black_box(data.as_slice()));
                black_box(hasher.result())
            })
        });
    }
    group.finish();
}

// ---------------------------------------------------------------- layer B

/// One slice, sweeping its length. Isolates the per-byte price.
fn syscall_by_length<H: HasherImpl>(c: &mut Criterion, name: &str) {
    let mut group = c.benchmark_group(format!("hash_{name}"));
    configure(&mut group);

    for &len in LENGTHS {
        let data = vec![0xa5u8; len];
        let slices = [VmSliceRaw {
            ptr: DATA_VA,
            len: len as u64,
        }];
        let mut result = [0u8; RESULT_LEN];
        let config = Config::default();

        prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
        let memory_mapping = unsafe {
            MemoryMapping::new(
                vec![
                    MemoryRegion::new(bytes_of_slice(&slices), VALS_VA),
                    MemoryRegion::new(bytes_of_mut(&mut result), RESULT_VA),
                    MemoryRegion::new(bytes_of_slice(data.as_slice()), DATA_VA),
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
            SyscallHash::<H>::rust(&mut invoke_context, VALS_VA, 1, RESULT_VA, 0, 0)
        );
        eprintln!("{name} len={len} slices=1 -> {cu} CU");

        invoke_context.compute_meter.mock_set_remaining(u64::MAX);
        group.throughput(Throughput::Elements(cu));
        group.bench_with_input(BenchmarkId::new("syscall", len), &len, |b, _| {
            b.iter(|| {
                black_box(
                    SyscallHash::<H>::rust(
                        &mut invoke_context,
                        black_box(VALS_VA),
                        black_box(1),
                        black_box(RESULT_VA),
                        0,
                        0,
                    )
                    .unwrap(),
                )
            })
        });
    }
    group.finish();
}

/// Many small slices. Isolates per-slice translation overhead and the
/// `mem_op_base_cost` floor.
fn syscall_by_slice_count<H: HasherImpl>(c: &mut Criterion, name: &str) {
    let max_slices = H::get_max_slices(&SVMTransactionExecutionBudget::default());
    let counts: Vec<usize> = [1usize, 2, 4, 8, 16, 32]
        .into_iter()
        .filter(|&k| k as u64 <= max_slices)
        .collect();

    let mut group = c.benchmark_group(format!("hash_{name}"));
    configure(&mut group);

    for &count in &counts {
        let data = vec![0xa5u8; COUNT_SWEEP_SLICE_LEN * count];
        let slices: Vec<VmSliceRaw> = (0..count)
            .map(|i| VmSliceRaw {
                ptr: DATA_VA + (i * COUNT_SWEEP_SLICE_LEN) as u64,
                len: COUNT_SWEEP_SLICE_LEN as u64,
            })
            .collect();
        let mut result = [0u8; RESULT_LEN];
        let config = Config::default();

        prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
        let memory_mapping = unsafe {
            MemoryMapping::new(
                vec![
                    MemoryRegion::new(bytes_of_slice(slices.as_slice()), VALS_VA),
                    MemoryRegion::new(bytes_of_mut(&mut result), RESULT_VA),
                    MemoryRegion::new(bytes_of_slice(data.as_slice()), DATA_VA),
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
            SyscallHash::<H>::rust(
                &mut invoke_context,
                VALS_VA,
                count as u64,
                RESULT_VA,
                0,
                0
            )
        );
        eprintln!("{name} len={COUNT_SWEEP_SLICE_LEN} slices={count} -> {cu} CU");

        invoke_context.compute_meter.mock_set_remaining(u64::MAX);
        group.throughput(Throughput::Elements(cu));
        group.bench_with_input(BenchmarkId::new("slices", count), &count, |b, _| {
            b.iter(|| {
                black_box(
                    SyscallHash::<H>::rust(
                        &mut invoke_context,
                        black_box(VALS_VA),
                        black_box(count as u64),
                        black_box(RESULT_VA),
                        0,
                        0,
                    )
                    .unwrap(),
                )
            })
        });
    }
    group.finish();
}

/// `vals_len = 0`: no slice array translated, no bytes hashed. Whatever this
/// costs is pure syscall overhead, and it is what `sha256_base_cost` has to cover.
fn syscall_base_only<H: HasherImpl>(c: &mut Criterion, name: &str) {
    let mut result = [0u8; RESULT_LEN];
    let config = Config::default();

    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![MemoryRegion::new(bytes_of_mut(&mut result), RESULT_VA)],
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
        SyscallHash::<H>::rust(&mut invoke_context, VALS_VA, 0, RESULT_VA, 0, 0)
    );
    eprintln!("{name} empty -> {cu} CU");

    invoke_context.compute_meter.mock_set_remaining(u64::MAX);
    let mut group = c.benchmark_group(format!("hash_{name}"));
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_function("base_only", |b| {
        b.iter(|| {
            black_box(
                SyscallHash::<H>::rust(
                    &mut invoke_context,
                    black_box(VALS_VA),
                    black_box(0),
                    black_box(RESULT_VA),
                    0,
                    0,
                )
                .unwrap(),
            )
        })
    });
    group.finish();
}

// ---------------------------------------------------------------- driver

macro_rules! for_each_hasher {
    ($c:expr, $($hasher:ty => $name:literal),+ $(,)?) => {
        $(
            primitive_by_length::<$hasher>($c, $name);
            syscall_base_only::<$hasher>($c, $name);
            syscall_by_length::<$hasher>($c, $name);
            syscall_by_slice_count::<$hasher>($c, $name);
        )+
    };
}

fn bench_hashes(c: &mut Criterion) {
    for_each_hasher!(
        c,
        Sha256Hasher => "sha256",
        Keccak256Hasher => "keccak256",
        Blake3Hasher => "blake3",
        Sha512Hasher => "sha512",
    );
}

criterion_group!(benches, bench_hashes);
criterion_main!(benches);
