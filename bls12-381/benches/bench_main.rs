#![allow(deprecated)]

mod test_vectors;

use {
    agave_bls12_381::*,
    bytemuck::pod_read_unaligned,
    criterion::{BenchmarkId, Criterion, criterion_group, criterion_main},
    test_vectors::*,
};

// Helper to construct pairing batches by repeating the single-pair test vector
fn build_pairing_input<const N: usize>(one_pair_vec: &[u8]) -> ([PodG1Point; N], [PodG2Point; N]) {
    // ONE_PAIR vector is structured as [G1 (96) | G2 (192)]
    let g1_bytes = &one_pair_vec[0..96];
    let g2_bytes = &one_pair_vec[96..288];

    let p1: PodG1Point = pod_read_unaligned(g1_bytes);
    let p2: PodG2Point = pod_read_unaligned(g2_bytes);

    let g1_arr = [p1; N];
    let g2_arr = [p2; N];

    (g1_arr, g2_arr)
}

fn bench_g1_ops(c: &mut Criterion) {
    let mut group = c.benchmark_group("G1 Operations");
    for endianness in [Endianness::BE, Endianness::LE] {
        let label = match endianness {
            Endianness::BE => "BE",
            Endianness::LE => "LE",
        };

        // Select static vectors based on endianness
        let (add_input, sub_input, mul_input, decompress_input, validate_input) = match endianness {
            Endianness::BE => (
                INPUT_BE_G1_ADD_WORST_CASE,
                INPUT_BE_G1_SUB_WORST_CASE,
                INPUT_BE_G1_MUL_WORST_CASE,
                INPUT_BE_G1_DECOMPRESS_WORST_CASE,
                INPUT_BE_G1_VALIDATE_WORST_CASE,
            ),
            Endianness::LE => (
                INPUT_LE_G1_ADD_WORST_CASE,
                INPUT_LE_G1_SUB_WORST_CASE,
                INPUT_LE_G1_MUL_WORST_CASE,
                INPUT_LE_G1_DECOMPRESS_WORST_CASE,
                INPUT_LE_G1_VALIDATE_WORST_CASE,
            ),
        };

        group.bench_function(BenchmarkId::new("Addition", label), |b| {
            let (p1_bytes, p2_bytes) = add_input.split_at(96);
            let p1: PodG1Point = pod_read_unaligned(p1_bytes);
            let p2: PodG1Point = pod_read_unaligned(p2_bytes);
            b.iter(|| bls12_381_g1_addition(Version::V0, &p1, &p2, endianness).unwrap())
        });

        group.bench_function(BenchmarkId::new("Subtraction", label), |b| {
            let (p1_bytes, p2_bytes) = sub_input.split_at(96);
            let p1: PodG1Point = pod_read_unaligned(p1_bytes);
            let p2: PodG1Point = pod_read_unaligned(p2_bytes);
            b.iter(|| bls12_381_g1_subtraction(Version::V0, &p1, &p2, endianness).unwrap())
        });

        group.bench_function(BenchmarkId::new("Multiplication", label), |b| {
            let (point_bytes, scalar_bytes) = mul_input.split_at(96);
            let point: PodG1Point = pod_read_unaligned(point_bytes);
            let scalar: PodScalar = pod_read_unaligned(scalar_bytes);
            b.iter(|| {
                bls12_381_g1_multiplication(Version::V0, &point, &scalar, endianness).unwrap()
            })
        });

        group.bench_function(BenchmarkId::new("Decompression", label), |b| {
            let input: PodG1Compressed = pod_read_unaligned(decompress_input);
            b.iter(|| bls12_381_g1_decompress(Version::V0, &input, endianness).unwrap())
        });

        group.bench_function(BenchmarkId::new("Validation", label), |b| {
            let input: PodG1Point = pod_read_unaligned(validate_input);
            b.iter(|| bls12_381_g1_point_validation(Version::V0, &input, endianness))
        });
    }
    group.finish();
}

fn bench_g2_ops(c: &mut Criterion) {
    let mut group = c.benchmark_group("G2 Operations");

    for endianness in [Endianness::BE, Endianness::LE] {
        let label = match endianness {
            Endianness::BE => "BE",
            Endianness::LE => "LE",
        };

        let (add_input, sub_input, mul_input, decompress_input, validate_input) = match endianness {
            Endianness::BE => (
                INPUT_BE_G2_ADD_WORST_CASE,
                INPUT_BE_G2_SUB_WORST_CASE,
                INPUT_BE_G2_MUL_WORST_CASE,
                INPUT_BE_G2_DECOMPRESS_WORST_CASE,
                INPUT_BE_G2_VALIDATE_WORST_CASE,
            ),
            Endianness::LE => (
                INPUT_LE_G2_ADD_WORST_CASE,
                INPUT_LE_G2_SUB_WORST_CASE,
                INPUT_LE_G2_MUL_WORST_CASE,
                INPUT_LE_G2_DECOMPRESS_WORST_CASE,
                INPUT_LE_G2_VALIDATE_WORST_CASE,
            ),
        };

        group.bench_function(BenchmarkId::new("Addition", label), |b| {
            let (p1_bytes, p2_bytes) = add_input.split_at(192);
            let p1: PodG2Point = pod_read_unaligned(p1_bytes);
            let p2: PodG2Point = pod_read_unaligned(p2_bytes);
            b.iter(|| bls12_381_g2_addition(Version::V0, &p1, &p2, endianness).unwrap())
        });

        group.bench_function(BenchmarkId::new("Subtraction", label), |b| {
            let (p1_bytes, p2_bytes) = sub_input.split_at(192);
            let p1: PodG2Point = pod_read_unaligned(p1_bytes);
            let p2: PodG2Point = pod_read_unaligned(p2_bytes);
            b.iter(|| bls12_381_g2_subtraction(Version::V0, &p1, &p2, endianness).unwrap())
        });

        group.bench_function(BenchmarkId::new("Multiplication", label), |b| {
            let (point_bytes, scalar_bytes) = mul_input.split_at(192);
            let point: PodG2Point = pod_read_unaligned(point_bytes);
            let scalar: PodScalar = pod_read_unaligned(scalar_bytes);
            b.iter(|| {
                bls12_381_g2_multiplication(Version::V0, &point, &scalar, endianness).unwrap()
            })
        });

        group.bench_function(BenchmarkId::new("Decompression", label), |b| {
            let input: PodG2Compressed = pod_read_unaligned(decompress_input);
            b.iter(|| bls12_381_g2_decompress(Version::V0, &input, endianness).unwrap())
        });

        group.bench_function(BenchmarkId::new("Validation", label), |b| {
            let input: PodG2Point = pod_read_unaligned(validate_input);
            b.iter(|| bls12_381_g2_point_validation(Version::V0, &input, endianness))
        });
    }
    group.finish();
}

macro_rules! bench_pair_size {
    ($group:expr, $N:expr) => {{
        // Construct inputs deterministically from the ONE_PAIR test vector
        let (g1_be, g2_be) = build_pairing_input::<$N>(INPUT_BE_PAIRING_WORST_CASE);
        let (g1_le, g2_le) = build_pairing_input::<$N>(INPUT_LE_PAIRING_WORST_CASE);

        // Bench BE
        $group.bench_with_input(BenchmarkId::new("BE", $N), &$N, |b, &_count| {
            b.iter(|| bls12_381_pairing_map(Version::V0, &g1_be, &g2_be, Endianness::BE).unwrap())
        });
        // Bench LE
        $group.bench_with_input(BenchmarkId::new("LE", $N), &$N, |b, &_count| {
            b.iter(|| bls12_381_pairing_map(Version::V0, &g1_le, &g2_le, Endianness::LE).unwrap())
        });
    }};
}

fn bench_pairing(c: &mut Criterion) {
    let mut group = c.benchmark_group("Pairing");

    bench_pair_size!(group, 1);
    bench_pair_size!(group, 2);
    bench_pair_size!(group, 4);
    bench_pair_size!(group, 8);
    bench_pair_size!(group, 16);
    group.finish();
}

criterion_group!(benches, bench_g1_ops, bench_g2_ops, bench_pairing);
criterion_main!(benches);
