#![allow(clippy::arithmetic_side_effects)]

use {
    bincode::{deserialize, serialize},
    criterion::{Criterion, criterion_group, criterion_main},
    prost::Message,
    solana_runtime::bank::RewardType,
    solana_transaction_status::{Reward, Rewards},
    std::hint::black_box,
};

fn create_rewards() -> Rewards {
    (0..100)
        .map(|i| Reward {
            pubkey: solana_pubkey::new_rand().to_string(),
            lamports: 42 + i,
            post_balance: u64::MAX,
            reward_type: Some(RewardType::Fee),
            commission: None,
            // `Reward::commission_bps` is `skip_serializing_if` without a serde
            // `default`, so it must be `Some` to round-trip through bincode.
            commission_bps: Some(i as u16),
        })
        .collect()
}

fn bincode_serialize_rewards(rewards: Rewards) -> Vec<u8> {
    serialize(&rewards).unwrap()
}

fn protobuf_serialize_rewards(rewards: Rewards) -> Vec<u8> {
    let rewards: solana_storage_proto::convert::generated::Rewards = rewards.into();
    let mut buffer = Vec::with_capacity(rewards.encoded_len());
    rewards.encode(&mut buffer).unwrap();
    buffer
}

fn bincode_deserialize_rewards(bytes: &[u8]) -> Rewards {
    deserialize(bytes).unwrap()
}

fn protobuf_deserialize_rewards(bytes: &[u8]) -> Rewards {
    solana_storage_proto::convert::generated::Rewards::decode(bytes)
        .unwrap()
        .into()
}

fn bench_serialize_rewards<S>(name: &str, c: &mut Criterion, serialize_method: S)
where
    S: Fn(Rewards) -> Vec<u8>,
{
    let rewards = create_rewards();
    c.bench_function(name, |b| {
        b.iter(|| {
            black_box(serialize_method(rewards.clone()));
        })
    });
}

fn bench_deserialize_rewards<S, D>(
    name: &str,
    c: &mut Criterion,
    serialize_method: S,
    deserialize_method: D,
) where
    S: Fn(Rewards) -> Vec<u8>,
    D: Fn(&[u8]) -> Rewards,
{
    let rewards = create_rewards();
    let rewards_bytes = serialize_method(rewards);
    c.bench_function(name, |b| {
        b.iter(|| {
            black_box(deserialize_method(&rewards_bytes));
        })
    });
}

fn bench_serialize_bincode(c: &mut Criterion) {
    bench_serialize_rewards("bench_serialize_bincode", c, bincode_serialize_rewards);
}

fn bench_serialize_protobuf(c: &mut Criterion) {
    bench_serialize_rewards("bench_serialize_protobuf", c, protobuf_serialize_rewards);
}

fn bench_deserialize_bincode(c: &mut Criterion) {
    bench_deserialize_rewards(
        "bench_deserialize_bincode",
        c,
        bincode_serialize_rewards,
        bincode_deserialize_rewards,
    );
}

fn bench_deserialize_protobuf(c: &mut Criterion) {
    bench_deserialize_rewards(
        "bench_deserialize_protobuf",
        c,
        protobuf_serialize_rewards,
        protobuf_deserialize_rewards,
    );
}

criterion_group!(
    benches,
    bench_serialize_bincode,
    bench_serialize_protobuf,
    bench_deserialize_bincode,
    bench_deserialize_protobuf
);
criterion_main!(benches);
