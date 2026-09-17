//! `sol_big_mod_exp`, per SIMD-0529.
//!
//! Cost model (charged in two parts by the implementation: a flat base cost up
//! front, then the operation cost after the exponent is read):
//!
//!   if decoded_exponent == 1:
//!       complexity = mult_complexity(max(base_len, modulus_len))
//!                    * MOD_REDUCTION_COMPLEXITY_FACTOR      (draft: 15)
//!   else:
//!       complexity = mult_complexity(max(base_len, modulus_len))
//!                    * max(adjusted_exponent_length, MIN_EXPONENT_LENGTH)
//!                                                            (draft: 75)
//!   cost = BASE_CU + ceil(complexity / CU_DIVISOR)            (draft: 422, 189)
//!
//!   mult_complexity(x) = x^2                       for x <= 64
//!                      = x^2/4 + 96x - 3072        for 64 < x <= 1024
//!                      = x^2/16 + 480x - 199680    for x > 1024
//!
//! SIMD-0529 states these constants are preliminary and MUST be finalised from
//! implementation benchmarks before activation, which is precisely what this
//! file is for.
//!
//! Three sweeps, chosen to hit the discontinuities in the model:
//!
//!   1. Operand size, at fixed exponent. Brackets the mult_complexity
//!      breakpoint at x = 64. Note that with MAX_BYTES = 512 the third branch
//!      of mult_complexity is unreachable.
//!   2. Exponent length, at fixed operand size. Brackets the
//!      MIN_EXPONENT_LENGTH clamp at 75 bits, where the price is flat below and
//!      linear above.
//!   3. The reduction path (exponent == 1) across operand sizes. Charged as if
//!      the exponent length were 15, versus a floor of 75 for every other
//!      exponent, so exponent 1 costs roughly a fifth of exponent 2 while doing
//!      strictly less work. Whether the real ratio is also 15/75 is the
//!      question.
//!
//! All inputs are little-endian. The modulus must be odd and greater than 1.

#[macro_use]
mod common;

use {
    common::*,
    criterion::{criterion_group, criterion_main, BenchmarkId},
    solana_syscalls::SyscallBigModExp,
    std::hint::black_box,
};

const PARAMS_VA: u64 = va(0);
const RESULT_VA: u64 = va(1);
const BASE_VA: u64 = va(2);
const EXPONENT_VA: u64 = va(3);
const MODULUS_VA: u64 = va(4);

/// `BIG_MOD_EXP_MAX_BYTES`.
const MAX_BYTES: usize = 512;

/// On-VM layout of `BigModExpParams`: six little-endian u64s, 48 bytes total.
#[repr(C)]
#[derive(Clone, Copy)]
struct ParamsRaw {
    base: u64,
    base_len: u64,
    exponent: u64,
    exponent_len: u64,
    modulus: u64,
    modulus_len: u64,
}

/// Deterministic filler. Not cryptographic; it only needs to be reproducible
/// and free of the special structure that all-ones or all-zeros would have.
fn pseudorandom(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        })
        .collect()
}

/// A full-length odd modulus. Little-endian, so byte 0 is least significant
/// (forced odd) and byte n-1 is most significant (top bit forced so the value
/// really occupies n bytes).
fn modulus(n: usize) -> Vec<u8> {
    let mut m = pseudorandom(n, 0xA5A5_1234);
    m[0] |= 1;
    m[n - 1] |= 0x80;
    m
}

fn base(n: usize) -> Vec<u8> {
    pseudorandom(n, 0x5EED_0001)
}

/// An exponent whose EIP-198 adjusted length is exactly `bits`.
///
/// `adjusted_exponent_length` is `index * 8 + (7 - leading_zeros(byte))` for
/// the most significant non-zero byte, indexed little-endian.
fn exponent_with_adjusted_bits(bits: u64) -> Vec<u8> {
    let index = (bits / 8) as usize;
    let bit_in_byte = (bits % 8) as u32;
    let mut e = pseudorandom(index + 1, 0xC0FF_EE01);
    // Clear everything above the target bit, then set it.
    e[index] = (1u8 << bit_in_byte) | (e[index] & ((1u8 << bit_in_byte) - 1));
    e
}

/// Exponent of `len` bytes, all bits set. Adjusted length is `8 * len - 1`.
fn exponent_full(len: usize) -> Vec<u8> {
    vec![0xffu8; len]
}

struct Case {
    name: String,
    base: Vec<u8>,
    exponent: Vec<u8>,
    modulus: Vec<u8>,
}

fn build_cases() -> Vec<Case> {
    let mut cases = Vec::new();

    // --- 1. Operand size sweep, fixed 32-byte full exponent (adjusted 255).
    for &n in &[32usize, 64, 65, 128, 256, 384, MAX_BYTES] {
        cases.push(Case {
            name: format!("size/n{n}"),
            base: base(n),
            exponent: exponent_full(32),
            modulus: modulus(n),
        });
    }

    // --- 2. Exponent sweep at fixed operand size.
    const N: usize = 256;
    // Below, at, and just above the MIN_EXPONENT_LENGTH clamp of 75 bits.
    for bits in [1u64, 32, 74, 75, 76, 128] {
        cases.push(Case {
            name: format!("exp/bits{bits}"),
            base: base(N),
            exponent: exponent_with_adjusted_bits(bits),
            modulus: modulus(N),
        });
    }
    // Full-length exponents: adjusted length 8*len - 1.
    for &len in &[32usize, 64, 128, 256, MAX_BYTES] {
        cases.push(Case {
            name: format!("exp/full{len}B"),
            base: base(N),
            exponent: exponent_full(len),
            modulus: modulus(N),
        });
    }

    // --- 3. Reduction path: decoded exponent exactly 1.
    for &n in &[32usize, 64, 128, 256, MAX_BYTES] {
        cases.push(Case {
            name: format!("reduce/n{n}"),
            base: base(n),
            exponent: vec![1u8],
            modulus: modulus(n),
        });
    }

    cases
}

fn bench_case(c: &mut Criterion, case: &Case) {
    let params = ParamsRaw {
        base: BASE_VA,
        base_len: case.base.len() as u64,
        exponent: EXPONENT_VA,
        exponent_len: case.exponent.len() as u64,
        modulus: MODULUS_VA,
        modulus_len: case.modulus.len() as u64,
    };
    let mut result = vec![0u8; case.modulus.len()];

    let config = Config::default();
    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![
                MemoryRegion::new(bytes_of(&params), PARAMS_VA),
                MemoryRegion::new(bytes_of_slice_mut(result.as_mut_slice()), RESULT_VA),
                MemoryRegion::new(bytes_of_slice(case.base.as_slice()), BASE_VA),
                MemoryRegion::new(bytes_of_slice(case.exponent.as_slice()), EXPONENT_VA),
                MemoryRegion::new(bytes_of_slice(case.modulus.as_slice()), MODULUS_VA),
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
        SyscallBigModExp::rust(&mut invoke_context, PARAMS_VA, RESULT_VA, 0, 0, 0)
    );
    eprintln!(
        "big_mod_exp {} (base={}B exp={}B mod={}B) -> {cu} CU",
        case.name,
        case.base.len(),
        case.exponent.len(),
        case.modulus.len()
    );

    invoke_context.compute_meter.mock_set_remaining(u64::MAX);

    // Group by sweep so criterion plots each one on its own axis.
    let (group_name, id) = case.name.split_once('/').expect("name is group/id");
    let mut group = c.benchmark_group(format!("big_mod_exp_{group_name}"));
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_function(BenchmarkId::new("syscall", id), |b| {
        b.iter(|| {
            black_box(
                SyscallBigModExp::rust(
                    &mut invoke_context,
                    black_box(PARAMS_VA),
                    black_box(RESULT_VA),
                    0,
                    0,
                    0,
                )
                .unwrap(),
            )
        })
    });
    group.finish();
}

/// Confirms the 512-byte limit is enforced on each length field.
fn probe_length_cap() {
    let n = MAX_BYTES + 1;
    let b = base(n);
    let e = exponent_full(32);
    let m = modulus(n);
    let params = ParamsRaw {
        base: BASE_VA,
        base_len: b.len() as u64,
        exponent: EXPONENT_VA,
        exponent_len: e.len() as u64,
        modulus: MODULUS_VA,
        modulus_len: m.len() as u64,
    };
    let mut result = vec![0u8; m.len()];

    let config = Config::default();
    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![
                MemoryRegion::new(bytes_of(&params), PARAMS_VA),
                MemoryRegion::new(bytes_of_slice_mut(result.as_mut_slice()), RESULT_VA),
                MemoryRegion::new(bytes_of_slice(b.as_slice()), BASE_VA),
                MemoryRegion::new(bytes_of_slice(e.as_slice()), EXPONENT_VA),
                MemoryRegion::new(bytes_of_slice(m.as_slice()), MODULUS_VA),
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

    let before = ContextObject::get_remaining(&invoke_context);
    let outcome = SyscallBigModExp::rust(&mut invoke_context, PARAMS_VA, RESULT_VA, 0, 0, 0);
    let charged = before - ContextObject::get_remaining(&invoke_context);
    match outcome {
        Err(_) => eprintln!(
            "big_mod_exp length cap ENFORCED (n={n} rejected), charged {charged} CU on the \
             reject path; SIMD-0529 says aborts before step 8 MUST NOT charge"
        ),
        Ok(status) => eprintln!(
            "big_mod_exp length cap NOT ENFORCED: n={n} returned status {status}; \
             SIMD-0529 requires a maximum of {MAX_BYTES}"
        ),
    }
}

fn bench_big_mod_exp(c: &mut Criterion) {
    probe_length_cap();
    for case in &build_cases() {
        bench_case(c, case);
    }
}

criterion_group!(benches, bench_big_mod_exp);
criterion_main!(benches);
