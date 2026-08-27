//! BLS12-381 validation, decompression, and group operations.
//!
//! Covered here:
//!   sol_curve_validate_point   -> bls12_381_g{1,2}_validate_cost
//!   sol_curve_decompress       -> bls12_381_g{1,2}_decompress_cost
//!   sol_curve_group_op add/sub/mul
//!                              -> bls12_381_g{1,2}_{add,subtract,multiply}_cost
//!
//! All are flat-priced, so the inputs are the implementers' worst-case vectors
//! (see `common/bls12_381.rs`): a flat price has to cover the slowest valid
//! input, not an average one.
//!
//! Note that the syscall calls `*_addition_unchecked` and
//! `*_subtraction_unchecked`, so add and sub perform no subgroup check.
//! Validation is a separate, separately priced syscall. The honest comparison
//! for a program that needs safety is therefore validate + op, not op alone.
//!
//! Layer A (the raw library functions on these same vectors) is deliberately
//! not duplicated here: the curve crate already ships it as
//! `cargo bench -p solana-bls12-381-syscall`. The difference between that and
//! the numbers below is the syscall overhead.

#[macro_use]
mod common;

use {
    common::{bls12_381::*, *},
    criterion::{criterion_group, criterion_main},
    solana_define_syscall::curve_constants::{
        BLS12_381_G1_BE, BLS12_381_G1_LE, BLS12_381_G2_BE, BLS12_381_G2_LE, GROUP_OP_ADD,
        GROUP_OP_MUL, GROUP_OP_SUB,
    },
    solana_syscalls::{SyscallCurveDecompress, SyscallCurveGroupOps, SyscallCurvePointValidation},
    std::hint::black_box,
};

const LEFT_VA: u64 = va(0);
const RIGHT_VA: u64 = va(1);
const RESULT_VA: u64 = va(2);

fn curve_id(g2: bool, le: bool) -> u64 {
    match (g2, le) {
        (false, false) => BLS12_381_G1_BE,
        (false, true) => BLS12_381_G1_LE,
        (true, false) => BLS12_381_G2_BE,
        (true, true) => BLS12_381_G2_LE,
    }
}

fn tag(g2: bool, le: bool) -> String {
    format!(
        "{}_{}",
        if g2 { "g2" } else { "g1" },
        if le { "le" } else { "be" }
    )
}

fn point_size(g2: bool) -> usize {
    if g2 {
        G2_POINT_SIZE
    } else {
        G1_POINT_SIZE
    }
}

// ---------------------------------------------------------------- validate

fn bench_validate(c: &mut Criterion, g2: bool, le: bool) {
    let id = curve_id(g2, le);
    let name = format!("bls12_381_{}_validate", tag(g2, le));

    // Both are bound unconditionally so the chosen one outlives the mapping.
    let p1 = g1_validate_input(le);
    let p2 = g2_validate_input(le);
    let point_bytes: *const [u8] = if g2 { bytes_of(&p2) } else { bytes_of(&p1) };

    let config = Config::default();
    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![MemoryRegion::new(point_bytes, LEFT_VA)],
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
        SyscallCurvePointValidation::rust(&mut invoke_context, id, LEFT_VA, 0, 0, 0)
    );
    eprintln!("{name} -> {cu} CU");

    invoke_context.compute_meter.mock_set_remaining(u64::MAX);
    let mut group = c.benchmark_group(name);
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_function("syscall", |b| {
        b.iter(|| {
            black_box(
                SyscallCurvePointValidation::rust(
                    &mut invoke_context,
                    black_box(id),
                    black_box(LEFT_VA),
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

// ---------------------------------------------------------------- decompress

fn bench_decompress(c: &mut Criterion, g2: bool, le: bool) {
    let id = curve_id(g2, le);
    let name = format!("bls12_381_{}_decompress", tag(g2, le));

    let c1 = g1_decompress_input(le);
    let c2 = g2_decompress_input(le);
    let input_bytes: *const [u8] = if g2 { bytes_of(&c2) } else { bytes_of(&c1) };
    let mut result = vec![0u8; point_size(g2)];

    let config = Config::default();
    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![
                MemoryRegion::new(input_bytes, LEFT_VA),
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
        SyscallCurveDecompress::rust(&mut invoke_context, id, LEFT_VA, RESULT_VA, 0, 0)
    );
    eprintln!("{name} -> {cu} CU");

    invoke_context.compute_meter.mock_set_remaining(u64::MAX);
    let mut group = c.benchmark_group(name);
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_function("syscall", |b| {
        b.iter(|| {
            black_box(
                SyscallCurveDecompress::rust(
                    &mut invoke_context,
                    black_box(id),
                    black_box(LEFT_VA),
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

// ---------------------------------------------------------------- group ops

fn bench_group_op(c: &mut Criterion, g2: bool, le: bool, op: u64, op_name: &str) {
    let id = curve_id(g2, le);
    let name = format!("bls12_381_{}_{op_name}", tag(g2, le));

    // Every fixture is bound before the selection below. Binding them inside
    // match arms would drop the tuples at the end of each arm while the raw
    // pointers taken from them are still live.
    let g1_add = g1_add_inputs(le);
    let g1_sub = g1_sub_inputs(le);
    let g1_mul = g1_mul_inputs(le);
    let g2_add = g2_add_inputs(le);
    let g2_sub = g2_sub_inputs(le);
    let g2_mul = g2_mul_inputs(le);

    // For MUL the left operand is the scalar and the right is the point.
    // For ADD and SUB both operands are points. The `*_mul_inputs` helpers
    // already return (scalar, point) in syscall order.
    let (left_bytes, right_bytes): (*const [u8], *const [u8]) = if op == GROUP_OP_ADD {
        if g2 {
            (bytes_of(&g2_add.0), bytes_of(&g2_add.1))
        } else {
            (bytes_of(&g1_add.0), bytes_of(&g1_add.1))
        }
    } else if op == GROUP_OP_SUB {
        if g2 {
            (bytes_of(&g2_sub.0), bytes_of(&g2_sub.1))
        } else {
            (bytes_of(&g1_sub.0), bytes_of(&g1_sub.1))
        }
    } else if g2 {
        (bytes_of(&g2_mul.0), bytes_of(&g2_mul.1))
    } else {
        (bytes_of(&g1_mul.0), bytes_of(&g1_mul.1))
    };

    let mut result = vec![0u8; point_size(g2)];

    let config = Config::default();
    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![
                MemoryRegion::new(left_bytes, LEFT_VA),
                MemoryRegion::new(right_bytes, RIGHT_VA),
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
        SyscallCurveGroupOps::rust(&mut invoke_context, id, op, LEFT_VA, RIGHT_VA, RESULT_VA)
    );
    eprintln!("{name} -> {cu} CU");

    invoke_context.compute_meter.mock_set_remaining(u64::MAX);
    let mut group = c.benchmark_group(name);
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_function("syscall", |b| {
        b.iter(|| {
            black_box(
                SyscallCurveGroupOps::rust(
                    &mut invoke_context,
                    black_box(id),
                    black_box(op),
                    black_box(LEFT_VA),
                    black_box(RIGHT_VA),
                    black_box(RESULT_VA),
                )
                .unwrap(),
            )
        })
    });
    group.finish();
}

// ---------------------------------------------------------------- driver

fn bench_bls12_381(c: &mut Criterion) {
    for g2 in [false, true] {
        for le in [false, true] {
            bench_validate(c, g2, le);
            bench_decompress(c, g2, le);
            for (op, op_name) in [
                (GROUP_OP_ADD, "add"),
                (GROUP_OP_SUB, "sub"),
                (GROUP_OP_MUL, "mul"),
            ] {
                bench_group_op(c, g2, le, op, op_name);
            }
        }
    }
}

criterion_group!(benches, bench_bls12_381);
criterion_main!(benches);
