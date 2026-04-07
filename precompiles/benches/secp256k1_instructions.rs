#![feature(test)]

extern crate test;
use {
    agave_feature_set::FeatureSet,
    agave_precompiles::secp256k1::verify,
    rand::Rng,
    solana_instruction::Instruction,
    solana_secp256k1_program::{
        eth_address_from_pubkey, new_secp256k1_instruction_with_signature, sign_message,
    },
    test::Bencher,
};

// 5K transactions should be enough for benching loop
const TX_COUNT: u16 = 5120;

// prepare a bunch of unique ixs
fn create_test_instructions(message_length: u16) -> Vec<Instruction> {
    let mut rng = rand::rng();
    (0..TX_COUNT)
        .map(|_| {
            let secret_bytes: [u8; 32] = rand::random();
            let secp_privkey = libsecp256k1::SecretKey::parse(&secret_bytes).unwrap();
            let message: Vec<u8> = (0..message_length)
                .map(|_| rng.random_range(0..255))
                .collect();
            let secp_pubkey = libsecp256k1::PublicKey::from_secret_key(&secp_privkey);
            let eth_address =
                eth_address_from_pubkey(&secp_pubkey.serialize()[1..].try_into().unwrap());
            let (signature, recovery_id) =
                sign_message(&secp_privkey.serialize(), &message).unwrap();
            new_secp256k1_instruction_with_signature(
                &message,
                &signature,
                recovery_id,
                &eth_address,
            )
        })
        .collect()
}

fn bench_verify(b: &mut Bencher, feature_set: &FeatureSet, message_length: u16) {
    let ixs = create_test_instructions(message_length);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], feature_set).unwrap();
    });
}

#[bench]
fn bench_secp256k1_legacy_len_032(b: &mut Bencher) {
    let feature_set = FeatureSet::default();
    bench_verify(b, &feature_set, 32);
}

#[bench]
fn bench_secp256k1_k256_len_032(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    bench_verify(b, &feature_set, 32);
}

#[bench]
fn bench_secp256k1_legacy_len_256(b: &mut Bencher) {
    let feature_set = FeatureSet::default();
    bench_verify(b, &feature_set, 256);
}

#[bench]
fn bench_secp256k1_k256_len_256(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    bench_verify(b, &feature_set, 256);
}

#[bench]
fn bench_secp256k1_legacy_len_32k(b: &mut Bencher) {
    let feature_set = FeatureSet::default();
    bench_verify(b, &feature_set, 32 * 1024);
}

#[bench]
fn bench_secp256k1_k256_len_32k(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    bench_verify(b, &feature_set, 32 * 1024);
}

#[bench]
fn bench_secp256k1_legacy_len_max(b: &mut Bencher) {
    let required_extra_space = 113_u16; // len for pubkey, sig, and offsets
    let feature_set = FeatureSet::default();
    bench_verify(b, &feature_set, u16::MAX - required_extra_space);
}

#[bench]
fn bench_secp256k1_k256_len_max(b: &mut Bencher) {
    let required_extra_space = 113_u16; // len for pubkey, sig, and offsets
    let feature_set = FeatureSet::all_enabled();
    bench_verify(b, &feature_set, u16::MAX - required_extra_space);
}
