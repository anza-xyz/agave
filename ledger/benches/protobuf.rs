#![allow(clippy::arithmetic_side_effects)]
#![feature(test)]
extern crate test;

use {
    bincode::{deserialize, serialize},
    prost::Message,
    solana_runtime::bank::RewardType,
    solana_transaction_status::{Reward, Rewards},
    test::Bencher,
};

fn create_rewards() -> Rewards {
    (0..100)
        .map(|i| Reward {
            pubkey: solana_pubkey::new_rand().to_string(),
            lamports: 42 + i,
            post_balance: u64::MAX,
            reward_type: Some(RewardType::Fee),
            commission: None,
            commission_bps: None,
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

fn bench_serialize_rewards<S>(bench: &mut Bencher, serialize_method: S)
where
    S: Fn(Rewards) -> Vec<u8>,
{
    let rewards = create_rewards();
    bench.iter(move || {
        let _ = serialize_method(rewards.clone());
    });
}

fn bench_deserialize_rewards<S, D>(bench: &mut Bencher, serialize_method: S, deserialize_method: D)
where
    S: Fn(Rewards) -> Vec<u8>,
    D: Fn(&[u8]) -> Rewards,
{
    let rewards = create_rewards();
    let rewards_bytes = serialize_method(rewards);
    bench.iter(move || {
        let _ = deserialize_method(&rewards_bytes);
    });
}

#[bench]
fn bench_serialize_bincode(bencher: &mut Bencher) {
    bench_serialize_rewards(bencher, bincode_serialize_rewards);
}

#[bench]
fn bench_serialize_protobuf(bencher: &mut Bencher) {
    bench_serialize_rewards(bencher, protobuf_serialize_rewards);
}

#[bench]
fn bench_deserialize_bincode(bencher: &mut Bencher) {
    bench_deserialize_rewards(
        bencher,
        bincode_serialize_rewards,
        bincode_deserialize_rewards,
    );
}

#[bench]
fn bench_deserialize_protobuf(bencher: &mut Bencher) {
    bench_deserialize_rewards(
        bencher,
        protobuf_serialize_rewards,
        protobuf_deserialize_rewards,
    );
}
