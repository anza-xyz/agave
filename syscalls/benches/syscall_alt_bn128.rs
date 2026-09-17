//! `sol_alt_bn128_group_op`: G1/G2 addition and multiplication, BE and LE.
//!
//! Each of these is flat-priced:
//!   alt_bn128_g1_addition_cost, alt_bn128_g2_addition_cost,
//!   alt_bn128_g1_multiplication_cost, alt_bn128_g2_multiplication_cost
//!
//! Flat pricing means the adversarial input is whatever is slowest, so the
//! multiplication cases sweep scalar bit length rather than using a fixed one.
//!
//! Pairing and compression live in their own file: their cost models scale
//! with input size and would muddy the fixed-cost comparison here.

#[macro_use]
mod common;

use {
    ark_bn254::{Fr, G1Affine, G1Projective, G2Affine, G2Projective},
    ark_ec::{AffineRepr, CurveGroup},
    ark_ff::PrimeField,
    common::{*, bn254::*},
    criterion::{criterion_group, criterion_main},
    solana_bn254::versioned::{
        alt_bn128_versioned_g1_addition, alt_bn128_versioned_g1_multiplication,
        alt_bn128_versioned_g2_addition, alt_bn128_versioned_g2_multiplication, Endianness,
        VersionedG1Addition, VersionedG1Multiplication, VersionedG2Addition,
        VersionedG2Multiplication, ALT_BN128_G1_ADD_BE, ALT_BN128_G1_ADD_LE, ALT_BN128_G1_MUL_BE,
        ALT_BN128_G1_MUL_LE, ALT_BN128_G1_POINT_SIZE, ALT_BN128_G2_ADD_BE, ALT_BN128_G2_ADD_LE,
        ALT_BN128_G2_MUL_BE, ALT_BN128_G2_MUL_LE, ALT_BN128_G2_POINT_SIZE,
    },
    solana_syscalls::SyscallAltBn128,
    std::hint::black_box,
};

const INPUT_VA: u64 = va(0);
const RESULT_VA: u64 = va(1);

// ------------------------------------------------------------ cases

struct Case {
    name: String,
    group_op: u64,
    input: Vec<u8>,
    output_len: usize,
    /// Layer A: the same operation with no VM around it.
    primitive: Box<dyn Fn(&[u8])>,
}

fn build_cases() -> Vec<Case> {
    let p1 = G1Affine::generator();
    let p2 = (G1Projective::from(p1) * Fr::from(7u64)).into_affine();
    let q1 = G2Affine::generator();
    let q2 = (G2Projective::from(q1) * Fr::from(7u64)).into_affine();

    // Flat price, so sweep the scalar. `r - 1` has full bit length and high
    // Hamming weight; `2` is the cheap end. The gap bounds what a flat price
    // has to cover.
    let k_2pow253 = {
        let mut bytes = [0u8; 32];
        bytes[31] = 0x20; // bit 253
        Fr::from_le_bytes_mod_order(&bytes)
    };
    let scalars: [(&str, Fr); 3] = [
        ("k2", Fr::from(2u64)),
        ("k_2pow253", k_2pow253),
        ("k_rminus1", -Fr::from(1u64)),
    ];

    let mut cases = Vec::new();

    for le in [false, true] {
        let tag = if le { "le" } else { "be" };

        // ---- G1 addition
        let mut input = Vec::new();
        input.extend_from_slice(&g1_bytes(&p1, le));
        input.extend_from_slice(&g1_bytes(&p2, le));
        cases.push(Case {
            name: format!("g1_add_{tag}"),
            group_op: if le {
                ALT_BN128_G1_ADD_LE
            } else {
                ALT_BN128_G1_ADD_BE
            },
            input,
            output_len: ALT_BN128_G1_POINT_SIZE,
            primitive: Box::new(move |i| {
                black_box(
                    alt_bn128_versioned_g1_addition(VersionedG1Addition::V0, i, endian(le))
                        .unwrap(),
                );
            }),
        });

        // ---- G2 addition
        let mut input = Vec::new();
        input.extend_from_slice(&g2_bytes(&q1, le));
        input.extend_from_slice(&g2_bytes(&q2, le));
        cases.push(Case {
            name: format!("g2_add_{tag}"),
            group_op: if le {
                ALT_BN128_G2_ADD_LE
            } else {
                ALT_BN128_G2_ADD_BE
            },
            input,
            output_len: ALT_BN128_G2_POINT_SIZE,
            primitive: Box::new(move |i| {
                black_box(
                    alt_bn128_versioned_g2_addition(VersionedG2Addition::V0, i, endian(le))
                        .unwrap(),
                );
            }),
        });

        for (sname, scalar) in &scalars {
            // ---- G1 multiplication
            let mut input = Vec::new();
            input.extend_from_slice(&g1_bytes(&p1, le));
            input.extend_from_slice(&fr_bytes(scalar, le));
            cases.push(Case {
                name: format!("g1_mul_{tag}_{sname}"),
                group_op: if le {
                    ALT_BN128_G1_MUL_LE
                } else {
                    ALT_BN128_G1_MUL_BE
                },
                input,
                output_len: ALT_BN128_G1_POINT_SIZE,
                primitive: Box::new(move |i| {
                    black_box(
                        alt_bn128_versioned_g1_multiplication(
                            VersionedG1Multiplication::V1,
                            i,
                            endian(le),
                        )
                        .unwrap(),
                    );
                }),
            });

            // ---- G2 multiplication
            let mut input = Vec::new();
            input.extend_from_slice(&g2_bytes(&q1, le));
            input.extend_from_slice(&fr_bytes(scalar, le));
            cases.push(Case {
                name: format!("g2_mul_{tag}_{sname}"),
                group_op: if le {
                    ALT_BN128_G2_MUL_LE
                } else {
                    ALT_BN128_G2_MUL_BE
                },
                input,
                output_len: ALT_BN128_G2_POINT_SIZE,
                primitive: Box::new(move |i| {
                    black_box(
                        alt_bn128_versioned_g2_multiplication(
                            VersionedG2Multiplication::V0,
                            i,
                            endian(le),
                        )
                        .unwrap(),
                    );
                }),
            });
        }
    }

    cases
}

// ------------------------------------------------------------ benches

fn bench_case(c: &mut Criterion, case: &Case) {
    let mut group = c.benchmark_group(format!("alt_bn128_{}", case.name));
    configure(&mut group);

    // Layer A
    group.bench_function("primitive", |b| {
        b.iter(|| (case.primitive)(black_box(case.input.as_slice())))
    });
    group.finish();

    // Layer B
    let mut result = vec![0u8; case.output_len];
    let input_len = case.input.len() as u64;
    let config = Config::default();

    prepare_mockup!(invoke_context, SVMFeatureSet::all_enabled());
    let memory_mapping = unsafe {
        MemoryMapping::new(
            vec![
                MemoryRegion::new(bytes_of_slice(case.input.as_slice()), INPUT_VA),
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
        SyscallAltBn128::rust(
            &mut invoke_context,
            case.group_op,
            INPUT_VA,
            input_len,
            RESULT_VA,
            0
        )
    );
    eprintln!("alt_bn128 {} -> {cu} CU", case.name);

    invoke_context.compute_meter.mock_set_remaining(u64::MAX);

    let mut group = c.benchmark_group(format!("alt_bn128_{}", case.name));
    configure(&mut group);
    group.throughput(Throughput::Elements(cu));
    group.bench_function("syscall", |b| {
        b.iter(|| {
            black_box(
                SyscallAltBn128::rust(
                    &mut invoke_context,
                    black_box(case.group_op),
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

fn bench_alt_bn128(c: &mut Criterion) {
    for case in &build_cases() {
        bench_case(c, case);
    }
}

criterion_group!(benches, bench_alt_bn128);
criterion_main!(benches);
