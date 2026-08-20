//! How much a `Shred::view()` costs, since every accessor takes one.

use {
    bytes::Bytes,
    criterion::{Criterion, Throughput, criterion_group, criterion_main},
    rand::{Rng, SeedableRng, rngs::StdRng},
    solana_shred::{
        Code, CodeShred, CommonHeader, Data, DataHeader, DataShred, Parsed, ShredFlags,
        ShredVariant, ShredViewMut,
        kind::ShredKind,
        wire_format::{OFFSET_OF_VARIANT, SIZE_OF_NONCE},
    },
    std::hint::black_box,
};

/// Number of distinct shreds each iteration walks, chosen so they do not all stay in caches.
const SHREDS: usize = 1024 * 64;

/// Random bytes of `K`'s payload length, with a valid variant byte of `K`'s kind at offset 64.
///
/// Every field but that byte is unconstrained: parsing reads the headers as scalars and validates
/// nothing about them.
fn random_shreds<K: ShredKind>(variant: ShredVariant) -> Vec<Bytes> {
    let mut rng = StdRng::seed_from_u64(0x5eed);
    (0..SHREDS)
        .map(|_| {
            let mut bytes = vec![0u8; K::SIZE_OF_PAYLOAD];
            rng.fill(&mut bytes[..]);
            bytes[OFFSET_OF_VARIANT] = u8::from(variant);
            Bytes::from(bytes)
        })
        .collect()
}

fn bench_view(c: &mut Criterion) {
    let data: Vec<_> = random_shreds::<Data>(ShredVariant::MerkleData)
        .into_iter()
        .map(|bytes| {
            DataShred::<Parsed>::parse(bytes)
                .expect("random bytes with a valid variant byte parse as a shred")
                .0
        })
        .collect();
    let code: Vec<_> = random_shreds::<Code>(ShredVariant::MerkleCode)
        .into_iter()
        .map(|bytes| {
            CodeShred::<Parsed>::parse(bytes)
                .expect("random bytes with a valid variant byte parse as a shred")
                .0
        })
        .collect();

    let mut group = c.benchmark_group("shred_view");
    group.throughput(Throughput::Elements(SHREDS as u64));
    group.bench_function("data", |b| {
        b.iter(|| {
            for shred in &data {
                black_box(shred.view());
            }
        })
    });
    group.bench_function("code", |b| {
        b.iter(|| {
            for shred in &code {
                black_box(shred.view());
            }
        })
    });
    // One accessor's worth of work: a view plus the span it cuts out of the shred.
    group.bench_function("data/erasure_shard", |b| {
        b.iter(|| {
            for shred in &data {
                black_box(shred.erasure_shard());
            }
        })
    });
    group.finish();
}

/// The writer's side of the same sections: what building one shred's worth of bytes costs.
fn bench_view_mut(c: &mut Criterion) {
    let common = CommonHeader {
        variant: ShredVariant::MerkleData,
        slot: 1_000,
        index: 64,
        version: 42,
        fec_set_index: 64,
    };
    let header = DataHeader {
        parent_offset: 1,
        flags: ShredFlags::from(9),
        size: 1_051,
    };
    let mut payloads = vec![vec![0u8; Data::SIZE_OF_PAYLOAD]; SHREDS];

    let mut group = c.benchmark_group("shred_view_mut");
    group.throughput(Throughput::Elements(SHREDS as u64));
    group.bench_function("data/write_headers", |b| {
        b.iter(|| {
            for payload in &mut payloads {
                ShredViewMut::<Data>::new(payload, ShredVariant::MerkleData)
                    .expect("the buffer is one data shred long")
                    .write_headers(&common, &header)
                    .expect("the headers fit the section they are written to");
            }
        })
    });
    // One section's worth of work, to compare against the read path's accessor.
    group.bench_function("data/erasure_shard", |b| {
        b.iter(|| {
            for payload in &mut payloads {
                let mut view = ShredViewMut::<Data>::new(payload, ShredVariant::MerkleData)
                    .expect("the buffer is one data shred long");
                black_box(view.erasure_shard_mut());
            }
        })
    });
    group.finish();
}

fn bench_parse(c: &mut Criterion) {
    // Half the packets carry a repair nonce, so the trailer branch is exercised too.
    let packets: Vec<_> = random_shreds::<Data>(ShredVariant::MerkleData)
        .into_iter()
        .enumerate()
        .map(|(index, shred)| match index % 2 {
            0 => shred,
            _ => {
                let mut packet = Vec::from(&shred[..]);
                packet.extend_from_slice(&[0xau8; SIZE_OF_NONCE]);
                Bytes::from(packet)
            }
        })
        .collect();

    let mut group = c.benchmark_group("shred_parse");
    group.throughput(Throughput::Elements(SHREDS as u64));
    group.bench_function("data", |b| {
        b.iter(|| {
            for packet in &packets {
                // Cloning `Bytes` is a refcount bump, so this measures the parse.
                black_box(DataShred::<Parsed>::parse(packet.clone()))
                    .expect("the packets were built to parse");
            }
        })
    });
    group.finish();
}

criterion_group!(benches, bench_view, bench_view_mut, bench_parse);
criterion_main!(benches);
