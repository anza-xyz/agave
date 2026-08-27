//! `sol_alt_bn128_compression`: G1/G2 compress and decompress, BE and LE.
//!
//! Flat-priced: syscall_base_cost + alt_bn128_g{1,2}_{compress,decompress}.
//!
//! Compression is a serialize plus a sign bit. Decompression solves y from x,
//! which is a square root in Fq (G1) or Fq2 (G2) and costs on the order of a
//! modular exponentiation. The four constants should be very unequal.

#[macro_use]
mod common;

use {
    ark_bn254::{G1Affine, G2Affine},
    ark_ec::AffineRepr,
    common::{bn254::*, *},
    criterion::{criterion_group, criterion_main},
    solana_bn254::{
        compression::prelude::{
            alt_bn128_g1_compress_be, alt_bn128_g1_compress_le, alt_bn128_g1_decompress_be,
            alt_bn128_g1_decompress_le, alt_bn128_g2_compress_be, alt_bn128_g2_compress_le,
            alt_bn128_g2_decompress_be, alt_bn128_g2_decompress_le,
            ALT_BN128_G1_COMPRESSED_POINT_SIZE, ALT_BN128_G1_COMPRESS_BE,
            ALT_BN128_G1_COMPRESS_LE, ALT_BN128_G1_DECOMPRESS_BE, ALT_BN128_G1_DECOMPRESS_LE,
            ALT_BN128_G2_COMPRESSED_POINT_SIZE, ALT_BN128_G2_COMPRESS_BE,
            ALT_BN128_G2_COMPRESS_LE, ALT_BN128_G2_DECOMPRESS_BE, ALT_BN128_G2_DECOMPRESS_LE,
        },
        versioned::{ALT_BN128_G1_POINT_SIZE, ALT_BN128_G2_POINT_SIZE},
    },
    solana_syscalls::SyscallAltBn128Compression,
    std::hint::black_box,
};

const INPUT_VA: u64 = va(0);
const RESULT_VA: u64 = va(1);

struct Case {
    name: String,
    op: u64,
    input: Vec<u8>,
    output_len: usize,
    primitive: Box<dyn Fn(&[u8])>,
}

fn build_cases() -> Vec<Case> {
    let mut cases = Vec::new();

    for le in [false, true] {
        let tag = if le { "le" } else { "be" };

        let g1_full = g1_bytes(&G1Affine::generator(), le).to_vec();
        let g2_full = g2_bytes(&G2Affine::generator(), le).to_vec();

        // Derive the compressed forms by round-tripping through the library
        // rather than guessing the flag encoding.
        let g1_comp = if le {
            alt_bn128_g1_compress_le(&g1_full).unwrap().to_vec()
        } else {
            alt_bn128_g1_compress_be(&g1_full).unwrap().to_vec()
        };
        let g2_comp = if le {
            alt_bn128_g2_compress_le(&g2_full).unwrap().to_vec()
        } else {
            alt_bn128_g2_compress_be(&g2_full).unwrap().to_vec()
        };

        cases.push(Case {
            name: format!("g1_compress_{tag}"),
            op: if le { ALT_BN128_G1_COMPRESS_LE } else { ALT_BN128_G1_COMPRESS_BE },
            input: g1_full.clone(),
            output_len: ALT_BN128_G1_COMPRESSED_POINT_SIZE,
            primitive: Box::new(move |i| {
                black_box(if le { alt_bn128_g1_compress_le(i) } else { alt_bn128_g1_compress_be(i) }.unwrap());
            }),
        });
        cases.push(Case {
            name: format!("g1_decompress_{tag}"),
            op: if le { ALT_BN128_G1_DECOMPRESS_LE } else { ALT_BN128_G1_DECOMPRESS_BE },
            input: g1_comp,
            output_len: ALT_BN128_G1_POINT_SIZE,
            primitive: Box::new(move |i| {
                black_box(if le { alt_bn128_g1_decompress_le(i) } else { alt_bn128_g1_decompress_be(i) }.unwrap());
            }),
        });
        cases.push(Case {
            name: format!("g2_compress_{tag}"),
            op: if le { ALT_BN128_G2_COMPRESS_LE } else { ALT_BN128_G2_COMPRESS_BE },
            input: g2_full.clone(),
            output_len: ALT_BN128_G2_COMPRESSED_POINT_SIZE,
            primitive: Box::new(move |i| {
                black_box(if le { alt_bn128_g2_compress_le(i) } else { alt_bn128_g2_compress_be(i) }.unwrap());
            }),
        });
        cases.push(Case {
            name: format!("g2_decompress_{tag}"),
            op: if le { ALT_BN128_G2_DECOMPRESS_LE } else { ALT_BN128_G2_DECOMPRESS_BE },
            input: g2_comp,
            output_len: ALT_BN128_G2_POINT_SIZE,
            primitive: Box::new(move |i| {
                black_box(if le { alt_bn128_g2_decompress_le(i) } else { alt_bn128_g2_decompress_be(i) }.unwrap());
            }),
        });
    }

    cases
}

fn bench_case(c: &mut Criterion, case: &Case) {
    let mut group = c.benchmark_group(format!("alt_bn128_{}", case.name));
    configure(&mut group);
    group.bench_function("primitive", |b| {
        b.iter(|| (case.primitive)(black_box(case.input.as_slice())))
    });
    group.finish();

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
        SyscallAltBn128Compression::rust(
            &mut invoke_context,
            case.op,
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
                SyscallAltBn128Compression::rust(
                    &mut invoke_context,
                    black_box(case.op),
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

fn bench_compression(c: &mut Criterion) {
    for case in &build_cases() {
        bench_case(c, case);
    }
}

criterion_group!(benches, bench_compression);
criterion_main!(benches);
