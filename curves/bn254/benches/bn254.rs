use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_bn254::prelude::{
        alt_bn128_addition, alt_bn128_addition_checked, alt_bn128_multiplication,
        alt_bn128_multiplication_checked, alt_bn128_pairing, alt_bn128_pairing_checked,
    },
};

// worst-case inputs for the checked encodings should be when at least one of
// the inputs is `p-1`

fn bench_addition_unchecked(c: &mut Criterion) {
    // big-endian byte encoding of the point `(p-1, 1)`
    let p_bytes = [
        37, 35, 100, 130, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19,
        167, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
    ];
    // big-endian byte encoding of the point `(p-1, 1)`
    let q_bytes = [
        37, 35, 100, 130, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19,
        167, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
    ];

    let input_bytes = [&p_bytes[..], &q_bytes[..]].concat();

    c.bench_function("unchecked bn128 addition", |b| {
        b.iter(|| alt_bn128_addition(&input_bytes))
    });
}

fn bench_addition_checked(c: &mut Criterion) {
    // big-endian byte encoding of the point `(p-1, 1)`
    let p_bytes = [
        37, 35, 100, 130, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19,
        167, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
    ];
    // big-endian byte encoding of the point `(p-1, 1)`
    let q_bytes = [
        37, 35, 100, 130, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19,
        167, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
    ];

    let input_bytes = [&p_bytes[..], &q_bytes[..]].concat();

    c.bench_function("checked bn128 addition", |b| {
        b.iter(|| alt_bn128_addition_checked(&input_bytes))
    });
}

fn bench_multiplication_unchecked(c: &mut Criterion) {
    // big-endian byte encoding of the point `(p-1, 1)`
    let point_bytes = [
        37, 35, 100, 130, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19,
        167, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
    ];
    // arbitrary 32 byte number
    let scalar_bytes = [
        1, 1, 1, 1, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19, 167, 0,
        0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 1,
    ];

    let input_bytes = [&point_bytes[..], &scalar_bytes[..]].concat();

    c.bench_function("unchecked bn128 multiplication", |b| {
        b.iter(|| alt_bn128_multiplication(&input_bytes))
    });
}

fn bench_multiplication_checked(c: &mut Criterion) {
    // big-endian byte encoding of the point `(p-1, 1)`
    let point_bytes = [
        37, 35, 100, 130, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19,
        167, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
    ];
    // arbitrary 32 byte number
    let scalar_bytes = [
        1, 1, 1, 1, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19, 167, 0,
        0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 1,
    ];

    let input_bytes = [&point_bytes[..], &scalar_bytes[..]].concat();

    c.bench_function("checked bn128 multiplication", |b| {
        b.iter(|| alt_bn128_multiplication_checked(&input_bytes))
    });
}

fn bench_pairing_unchecked(c: &mut Criterion) {
    // big-endian byte encoding of the point `(p-1, 1)`
    let p_bytes = [
        37, 35, 100, 130, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19,
        167, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
    ];
    // big-endian encoding of an arbitrary byte in G2
    let q_bytes = [
        25, 142, 147, 147, 146, 13, 72, 58, 114, 96, 191, 183, 49, 251, 93, 37, 241, 170, 73, 51,
        53, 169, 231, 18, 151, 228, 133, 183, 174, 243, 18, 194, 24, 0, 222, 239, 18, 31, 30, 118,
        66, 106, 0, 102, 94, 92, 68, 121, 103, 67, 34, 212, 247, 94, 218, 221, 70, 222, 189, 92,
        217, 146, 246, 237,
    ];

    let input_bytes = [&p_bytes[..], &q_bytes[..]].concat();

    c.bench_function("unchecked bn128 pairing", |b| {
        b.iter(|| alt_bn128_pairing(&input_bytes))
    });
}

fn bench_pairing_checked(c: &mut Criterion) {
    // big-endian byte encoding of the point `(p-1, 1)`
    let p_bytes = [
        37, 35, 100, 130, 64, 0, 0, 1, 186, 52, 77, 128, 0, 0, 0, 8, 97, 33, 0, 0, 0, 0, 0, 19,
        167, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
    ];
    // big-endian encoding of an arbitrary byte in G2
    let q_bytes = [
        25, 142, 147, 147, 146, 13, 72, 58, 114, 96, 191, 183, 49, 251, 93, 37, 241, 170, 73, 51,
        53, 169, 231, 18, 151, 228, 133, 183, 174, 243, 18, 194, 24, 0, 222, 239, 18, 31, 30, 118,
        66, 106, 0, 102, 94, 92, 68, 121, 103, 67, 34, 212, 247, 94, 218, 221, 70, 222, 189, 92,
        217, 146, 246, 237,
    ];

    let input_bytes = [&p_bytes[..], &q_bytes[..]].concat();

    c.bench_function("checked bn128 pairing", |b| {
        b.iter(|| alt_bn128_pairing_checked(&input_bytes))
    });
}

criterion_group!(
    benches,
    bench_addition_unchecked,
    bench_addition_checked,
    bench_multiplication_unchecked,
    bench_multiplication_checked,
    bench_pairing_unchecked,
    bench_pairing_checked,
);
criterion_main!(benches);
