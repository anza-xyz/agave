//! How much a `Shred::view()` costs, since every accessor takes one, and what the kind-erased
//! `AnyShred` adds on top of it.

use {
    bytes::Bytes,
    criterion::{Criterion, Throughput, criterion_group, criterion_main},
    rand::{Rng, SeedableRng, rngs::StdRng},
    solana_shred::{
        headers::{CodeHeader, CommonHeader, DataHeader, ShredFlags},
        kind::{Code, Data, ShredLayout},
        shred::{AnyShred, CodeShred, DataShred, parse_turbine},
        shred_variant::ShredVariant,
        state::Parsed,
        view::ShredViewMut,
        wire_format::SIZE_OF_NONCE,
    },
    std::hint::black_box,
};

/// Number of distinct shreds each iteration walks, chosen so they do not all stay in caches.
const SHREDS: usize = 1024 * 64;

/// Random bytes of `K`'s payload length, with real headers written over them.
///
/// The bodies are unconstrained, since nothing downstream of the headers reads them, but the headers
/// themselves have to be the ones a writer would produce: a data shred's `size` is validated against
/// the layout while the shred is read, so random bytes there do not parse. Writing them with
/// [`ShredViewMut`] is also what keeps the layout out of this file.
fn random_shreds<K: ShredLayout>(common: &CommonHeader, header: &K::Header) -> Vec<Bytes> {
    let mut rng = StdRng::seed_from_u64(0x5eed);
    (0..SHREDS)
        .map(|_| {
            let mut payload = vec![0u8; K::SIZE_OF_PAYLOAD];
            rng.fill(&mut payload[..]);
            ShredViewMut::<K>::new(&mut payload, common.variant)
                .expect("the buffer is one shred of this kind long")
                .write_headers(common, header)
                .expect("the headers fit the section they are written to");
            Bytes::from(payload)
        })
        .collect()
}

/// Headers of the first data shred of a full FEC set, whose `size` covers the whole body.
fn data_headers(variant: ShredVariant) -> (CommonHeader, DataHeader) {
    let common = CommonHeader {
        variant,
        slot: 1_000,
        index: 64,
        version: 42,
        fec_set_index: 64,
    };
    let header = DataHeader {
        parent_offset: 1,
        flags: ShredFlags::from(9),
        size: u16::try_from(Data::SIZE_OF_HEADERS.saturating_add(Data::SIZE_OF_BODY))
            .expect("a data shred's length fits a u16"),
    };
    (common, header)
}

/// Headers of the first code shred of a full FEC set.
fn code_headers(variant: ShredVariant) -> (CommonHeader, CodeHeader) {
    let common = CommonHeader {
        variant,
        slot: 1_000,
        index: 96,
        version: 42,
        fec_set_index: 64,
    };
    let header = CodeHeader {
        num_data_shreds: 32,
        num_code_shreds: 32,
        position: 0,
    };
    (common, header)
}

fn bench_view(c: &mut Criterion) {
    let (common, header) = data_headers(ShredVariant::MerkleData);
    let data: Vec<_> = random_shreds::<Data>(&common, &header)
        .into_iter()
        .map(|bytes| {
            DataShred::<Parsed>::parse_turbine(bytes)
                .expect("random bytes with a valid variant byte parse as a shred")
        })
        .collect();
    let (code_common, code_header) = code_headers(ShredVariant::MerkleCode);
    let code: Vec<_> = random_shreds::<Code>(&code_common, &code_header)
        .into_iter()
        .map(|bytes| {
            CodeShred::<Parsed>::parse_turbine(bytes)
                .expect("random bytes with a valid variant byte parse as a shred")
        })
        .collect();

    // The same data shreds with the kind erased, so the two `view()` rows are directly comparable:
    // the erased one pays for a match on the header discriminant before it can pick a `K`.
    let erased: Vec<AnyShred<Parsed>> = data.iter().cloned().map(AnyShred::from).collect();

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
    group.bench_function("erased", |b| {
        b.iter(|| {
            for shred in &erased {
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
    let (common, header) = data_headers(ShredVariant::MerkleData);
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
    let (common, header) = data_headers(ShredVariant::MerkleData);
    let turbine = random_shreds::<Data>(&common, &header);
    // A repair response is the same shred with the nonce of the request it answers behind it, and
    // its own entry point, so both doors are measured.
    let repair: Vec<_> = turbine
        .iter()
        .map(|shred| {
            let mut packet = Vec::from(&shred[..]);
            packet.extend_from_slice(&[0xau8; SIZE_OF_NONCE]);
            Bytes::from(packet)
        })
        .collect();

    let mut group = c.benchmark_group("shred_parse");
    group.throughput(Throughput::Elements(SHREDS as u64));
    group.bench_function("data", |b| {
        b.iter(|| {
            for packet in &turbine {
                // Cloning `Bytes` is a refcount bump, so this measures the parse.
                black_box(DataShred::<Parsed>::parse_turbine(packet.clone()))
                    .expect("the packets were built to parse");
            }
        })
    });
    group.bench_function("data/repair", |b| {
        b.iter(|| {
            for packet in &repair {
                black_box(DataShred::<Parsed>::parse_repair(packet.clone()))
                    .expect("the packets were built to parse");
            }
        })
    });
    // What a socket-reading worker actually calls: the same parse, plus the variant peek that
    // picks the kind and the erasure that follows it.
    group.bench_function("erased", |b| {
        b.iter(|| {
            for packet in &turbine {
                black_box(parse_turbine(packet.clone())).expect("the packets were built to parse");
            }
        })
    });
    group.finish();
}

criterion_group!(benches, bench_view, bench_view_mut, bench_parse);
criterion_main!(benches);
