//! `sol_poseidon`.
//!
//! Cost is `execution_cost.poseidon_cost(vals_len)`, which is quadratic in the
//! number of inputs. The syscall hard-caps `vals_len` at 12, so the whole
//! domain is measurable: this file sweeps every value from 1 to 12 and both
//! endiannesses, giving 24 points against a two-parameter quadratic.
//!
//! Every input must be a canonical BN254 field element (32 bytes, less than r).
//! A non-canonical input makes `poseidon::hashv` fail and the syscall return
//! status 1 via the cheap error path, which `charged_cu!` catches.

#[macro_use]
mod common;

use {
    common::*,
    criterion::{criterion_group, criterion_main, BenchmarkId},
    solana_syscalls::SyscallPoseidon,
    std::hint::black_box,
};

const VALS_VA: u64 = va(0);
const RESULT_VA: u64 = va(1);
const DATA_VA: u64 = va(2);

/// The syscall rejects anything above this.
const MAX_INPUTS: usize = 12;
/// One BN254 field element.
const FIELD_BYTES: usize = 32;
/// `poseidon::HASH_BYTES`.
const HASH_BYTES: usize = 32;

/// `poseidon::Parameters::Bn254X5`, the only variant. The syscall does
/// `u64::try_into()` and errors on anything else, so a wrong value here shows
/// up immediately as a failed `charged_cu!` probe rather than silently
/// benchmarking something else.
const PARAM_BN254_X5: u64 = 0;
/// `poseidon::Endianness::BigEndian` / `::LittleEndian`.
const ENDIAN_BE: u64 = 0;
const ENDIAN_LE: u64 = 1;

/// `n` distinct canonical field elements.
///
/// Byte 0 and byte 31 are both zeroed so the value stays below the BN254 group
/// order whichever endianness the syscall is told to use.
fn field_elements(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n * FIELD_BYTES];
    for (i, chunk) in out.chunks_mut(FIELD_BYTES).enumerate() {
        chunk.fill(0x11);
        chunk[8..16].copy_from_slice(&(i as u64 + 1).to_le_bytes());
        chunk[0] = 0;
        chunk[FIELD_BYTES - 1] = 0;
    }
    out
}

fn bench_one(c: &mut Criterion, n: usize, le: bool) {
    let endianness = if le { ENDIAN_LE } else { ENDIAN_BE };
    let tag = if le { "le" } else { "be" };
    let name = format!("poseidon_{tag}");

    let data = field_elements(n);
    let slices: Vec<VmSliceRaw> = (0..n)
        .map(|i| VmSliceRaw {
            ptr: DATA_VA + (i * FIELD_BYTES) as u64,
            len: FIELD_BYTES as u64,
        })
        .collect();
    let mut result = [0u8; HASH_BYTES];

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
        SyscallPoseidon::rust(
            &mut invoke_context,
            PARAM_BN254_X5,
            endianness,
            VALS_VA,
            n as u64,
            RESULT_VA
        )
    );
    eprintln!("poseidon_{tag} n={n} -> {cu} CU");

    invoke_context.compute_meter.mock_set_remaining(u64::MAX);

    let mut group = c.benchmark_group(name);
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_with_input(BenchmarkId::new("syscall", n), &n, |b, _| {
        b.iter(|| {
            black_box(
                SyscallPoseidon::rust(
                    &mut invoke_context,
                    black_box(PARAM_BN254_X5),
                    black_box(endianness),
                    black_box(VALS_VA),
                    black_box(n as u64),
                    black_box(RESULT_VA),
                )
                .unwrap(),
            )
        })
    });
    group.finish();
}

/// Confirms the syscall rejects `vals_len = 13`. Not a benchmark: runs once
/// and prints.
fn probe_input_cap(le: bool) {
    let over = MAX_INPUTS + 1;
    let endianness = if le { ENDIAN_LE } else { ENDIAN_BE };
    let data = field_elements(over);
    let slices: Vec<VmSliceRaw> = (0..over)
        .map(|i| VmSliceRaw {
            ptr: DATA_VA + (i * FIELD_BYTES) as u64,
            len: FIELD_BYTES as u64,
        })
        .collect();
    let mut result = [0u8; HASH_BYTES];

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
    invoke_context.compute_meter.mock_set_remaining(u64::MAX);

    let outcome = SyscallPoseidon::rust(
        &mut invoke_context,
        PARAM_BN254_X5,
        endianness,
        VALS_VA,
        over as u64,
        RESULT_VA,
    );
    match outcome {
        Err(_) => eprintln!("poseidon input cap ENFORCED (vals_len={over} rejected)"),
        Ok(status) => eprintln!(
            "poseidon input cap NOT ENFORCED: vals_len={over} returned status {status}"
        ),
    }
}

fn bench_poseidon(c: &mut Criterion) {
    probe_input_cap(false);
    for le in [false, true] {
        for n in 1..=MAX_INPUTS {
            bench_one(c, n, le);
        }
    }
}

criterion_group!(benches, bench_poseidon);
criterion_main!(benches);
