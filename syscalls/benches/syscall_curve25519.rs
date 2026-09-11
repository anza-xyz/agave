//! `sol_curve_validate_point`, `sol_curve_group_op`, `sol_curve_multiscalar_mul`
//! for the Edwards and Ristretto representations of curve25519.
//!
//! Validation, add, sub and mul are flat-priced. MSM is two-term:
//!   msm_base_cost + msm_incremental_cost * (n - 1)
//!
//! The interesting property here is that curve25519-dalek changes algorithm
//! with n (Straus for small n, Pippenger above a threshold), while the price is
//! strictly linear. A linear price cannot track a piecewise-linear cost, so the
//! sweep below deliberately brackets the crossover.

#[macro_use]
mod common;

use {
    common::*,
    criterion::{criterion_group, criterion_main, BenchmarkId},
    solana_curve25519::{
        edwards::{self, PodEdwardsPoint},
        ristretto::{self, PodRistrettoPoint},
        scalar::PodScalar,
    },
    solana_define_syscall::curve_constants::{
        CURVE25519_EDWARDS, CURVE25519_RISTRETTO, GROUP_OP_ADD, GROUP_OP_MUL, GROUP_OP_SUB,
    },
    solana_syscalls::{
        SyscallCurveGroupOps, SyscallCurveMultiscalarMultiplication, SyscallCurvePointValidation,
    },
    std::hint::black_box,
};

const LEFT_VA: u64 = va(0);
const RIGHT_VA: u64 = va(1);
const RESULT_VA: u64 = va(2);
const SCALARS_VA: u64 = va(3);
const POINTS_VA: u64 = va(4);

/// The syscall rejects anything above this.
const MAX_MSM_POINTS: usize = 512;

/// Brackets dalek's Straus/Pippenger crossover, which sits somewhere in the
/// 100-200 range depending on version and backend.
const MSM_COUNTS: &[usize] = &[1, 2, 4, 8, 16, 32, 64, 128, 190, 256, 384, 512];

// ------------------------------------------------------------ curve abstraction

trait Curve {
    const ID: u64;
    const NAME: &'static str;
    type Point: Copy;

    /// A known-valid point, taken verbatim from the crate's own tests.
    fn base() -> Self::Point;

    fn validate(p: &Self::Point) -> bool;
    fn add(a: &Self::Point, b: &Self::Point) -> Option<Self::Point>;
    fn sub(a: &Self::Point, b: &Self::Point) -> Option<Self::Point>;
    fn mul(s: &PodScalar, p: &Self::Point) -> Option<Self::Point>;
    fn msm(s: &[PodScalar], p: &[Self::Point]) -> Option<Self::Point>;
}

struct Edwards;
impl Curve for Edwards {
    const ID: u64 = CURVE25519_EDWARDS;
    const NAME: &'static str = "edwards";
    type Point = PodEdwardsPoint;

    fn base() -> Self::Point {
        PodEdwardsPoint([
            201, 179, 241, 122, 180, 185, 239, 50, 183, 52, 221, 0, 153, 195, 43, 18, 22, 38, 187,
            206, 179, 192, 210, 58, 53, 45, 150, 98, 89, 17, 158, 11,
        ])
    }
    fn validate(p: &Self::Point) -> bool {
        edwards::validate_edwards(p)
    }
    fn add(a: &Self::Point, b: &Self::Point) -> Option<Self::Point> {
        edwards::add_edwards(a, b)
    }
    fn sub(a: &Self::Point, b: &Self::Point) -> Option<Self::Point> {
        edwards::subtract_edwards(a, b)
    }
    fn mul(s: &PodScalar, p: &Self::Point) -> Option<Self::Point> {
        edwards::multiply_edwards(s, p)
    }
    fn msm(s: &[PodScalar], p: &[Self::Point]) -> Option<Self::Point> {
        edwards::multiscalar_multiply_edwards(s, p)
    }
}

struct Ristretto;
impl Curve for Ristretto {
    const ID: u64 = CURVE25519_RISTRETTO;
    const NAME: &'static str = "ristretto";
    type Point = PodRistrettoPoint;

    fn base() -> Self::Point {
        PodRistrettoPoint([
            226, 242, 174, 10, 106, 188, 78, 113, 168, 132, 169, 97, 197, 0, 81, 95, 88, 227, 11,
            106, 165, 130, 221, 141, 182, 166, 89, 69, 224, 141, 45, 118,
        ])
    }
    fn validate(p: &Self::Point) -> bool {
        ristretto::validate_ristretto(p)
    }
    fn add(a: &Self::Point, b: &Self::Point) -> Option<Self::Point> {
        ristretto::add_ristretto(a, b)
    }
    fn sub(a: &Self::Point, b: &Self::Point) -> Option<Self::Point> {
        ristretto::subtract_ristretto(a, b)
    }
    fn mul(s: &PodScalar, p: &Self::Point) -> Option<Self::Point> {
        ristretto::multiply_ristretto(s, p)
    }
    fn msm(s: &[PodScalar], p: &[Self::Point]) -> Option<Self::Point> {
        ristretto::multiscalar_multiply_ristretto(s, p)
    }
}

// ------------------------------------------------------------ input generation

/// A canonical scalar (top byte 0x0f keeps it below the group order) with
/// every other byte set, so the Hamming weight is near maximal. Flat-priced
/// multiplication has to cover the slowest scalar, not an average one.
fn heavy_scalar(seed: u8) -> PodScalar {
    let mut bytes = [0xffu8; 32];
    bytes[0] = seed;
    bytes[31] = 0x0f;
    PodScalar(bytes)
}

/// Distinct valid points, derived from the known-good base by scalar
/// multiplication. Every one is a real curve point, so no bench ever ends up
/// on the cheap rejection path.
fn distinct_points<C: Curve>(n: usize) -> Vec<C::Point> {
    let base = C::base();
    (0..n)
        .map(|i| {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&(i as u64 + 2).to_le_bytes());
            C::mul(&PodScalar(bytes), &base)
                .unwrap_or_else(|| panic!("{} point derivation failed at {i}", C::NAME))
        })
        .collect()
}

fn heavy_scalars(n: usize) -> Vec<PodScalar> {
    (0..n).map(|i| heavy_scalar(i as u8 | 1)).collect()
}

// ------------------------------------------------------------ validation

fn bench_validate<C: Curve>(c: &mut Criterion) {
    let point = C::base();
    assert!(C::validate(&point), "{} base point is invalid", C::NAME);

    let mut group = c.benchmark_group(format!("curve25519_{}_validate", C::NAME));
    configure(&mut group);
    group.bench_function("primitive", |b| {
        b.iter(|| black_box(C::validate(black_box(&point))))
    });
    group.finish();

    let config = Config::default();
    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![MemoryRegion::new(bytes_of(&point), LEFT_VA)],
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
        SyscallCurvePointValidation::rust(&mut invoke_context, C::ID, LEFT_VA, 0, 0, 0)
    );
    eprintln!("curve25519 {}_validate -> {cu} CU", C::NAME);

    invoke_context.compute_meter.mock_set_remaining(u64::MAX);
    let mut group = c.benchmark_group(format!("curve25519_{}_validate", C::NAME));
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_function("syscall", |b| {
        b.iter(|| {
            black_box(
                SyscallCurvePointValidation::rust(
                    &mut invoke_context,
                    black_box(C::ID),
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

// ------------------------------------------------------------ group ops

fn bench_group_op<C: Curve>(c: &mut Criterion, op: u64, op_name: &str) {
    let points = distinct_points::<C>(2);
    let scalar = heavy_scalar(0x7f);

    // For MUL the left operand is the scalar and the right is the point.
    // For ADD and SUB both operands are points.
    let left_bytes: *const [u8] = if op == GROUP_OP_MUL {
        bytes_of(&scalar)
    } else {
        bytes_of(&points[0])
    };

    let mut result = [0u8; 32];
    let config = Config::default();

    let mut group = c.benchmark_group(format!("curve25519_{}_{op_name}", C::NAME));
    configure(&mut group);
    group.bench_function("primitive", |b| {
        b.iter(|| match op {
            GROUP_OP_ADD => black_box(C::add(black_box(&points[0]), black_box(&points[1])).is_some()),
            GROUP_OP_SUB => black_box(C::sub(black_box(&points[0]), black_box(&points[1])).is_some()),
            _ => black_box(C::mul(black_box(&scalar), black_box(&points[1])).is_some()),
        })
    });
    group.finish();

    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![
                MemoryRegion::new(left_bytes, LEFT_VA),
                MemoryRegion::new(bytes_of(&points[1]), RIGHT_VA),
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
        SyscallCurveGroupOps::rust(
            &mut invoke_context,
            C::ID,
            op,
            LEFT_VA,
            RIGHT_VA,
            RESULT_VA
        )
    );
    eprintln!("curve25519 {}_{op_name} -> {cu} CU", C::NAME);

    invoke_context.compute_meter.mock_set_remaining(u64::MAX);
    let mut group = c.benchmark_group(format!("curve25519_{}_{op_name}", C::NAME));
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_function("syscall", |b| {
        b.iter(|| {
            black_box(
                SyscallCurveGroupOps::rust(
                    &mut invoke_context,
                    black_box(C::ID),
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

// ------------------------------------------------------------ MSM

fn bench_msm<C: Curve>(c: &mut Criterion) {
    // Layer A
    let mut group = c.benchmark_group(format!("curve25519_{}_msm", C::NAME));
    configure(&mut group);
    for &n in MSM_COUNTS {
        let scalars = heavy_scalars(n);
        let points = distinct_points::<C>(n);
        group.bench_with_input(BenchmarkId::new("primitive", n), &n, |b, _| {
            b.iter(|| {
                black_box(
                    C::msm(black_box(scalars.as_slice()), black_box(points.as_slice())).unwrap(),
                )
            })
        });
    }
    group.finish();

    // Layer B
    for &n in MSM_COUNTS {
        assert!(n <= MAX_MSM_POINTS);
        let scalars = heavy_scalars(n);
        let points = distinct_points::<C>(n);
        let mut result = [0u8; 32];
        let config = Config::default();

        prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
        let memory_mapping = unsafe {
            MemoryMapping::new(
                vec![
                    MemoryRegion::new(bytes_of_slice(scalars.as_slice()), SCALARS_VA),
                    MemoryRegion::new(bytes_of_slice(points.as_slice()), POINTS_VA),
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
            SyscallCurveMultiscalarMultiplication::rust(
                &mut invoke_context,
                C::ID,
                SCALARS_VA,
                POINTS_VA,
                n as u64,
                RESULT_VA
            )
        );
        eprintln!("curve25519 {}_msm n={n} -> {cu} CU", C::NAME);

        invoke_context.compute_meter.mock_set_remaining(u64::MAX);
        let mut group = c.benchmark_group(format!("curve25519_{}_msm", C::NAME));
        configure(&mut group);
        group.throughput(Throughput::Elements(cu));
        group.bench_with_input(BenchmarkId::new("syscall", n), &n, |b, _| {
            b.iter(|| {
                black_box(
                    SyscallCurveMultiscalarMultiplication::rust(
                        &mut invoke_context,
                        black_box(C::ID),
                        black_box(SCALARS_VA),
                        black_box(POINTS_VA),
                        black_box(n as u64),
                        black_box(RESULT_VA),
                    )
                    .unwrap(),
                )
            })
        });
        group.finish();
    }
}

// ------------------------------------------------------------ driver

fn bench_curve<C: Curve>(c: &mut Criterion) {
    bench_validate::<C>(c);
    bench_group_op::<C>(c, GROUP_OP_ADD, "add");
    bench_group_op::<C>(c, GROUP_OP_SUB, "sub");
    bench_group_op::<C>(c, GROUP_OP_MUL, "mul");
    bench_msm::<C>(c);
}

fn bench_curve25519(c: &mut Criterion) {
    bench_curve::<Edwards>(c);
    bench_curve::<Ristretto>(c);
}

criterion_group!(benches, bench_curve25519);
criterion_main!(benches);
