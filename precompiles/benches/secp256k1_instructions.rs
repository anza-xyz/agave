#![feature(test)]

extern crate test;
use {
    agave_feature_set::FeatureSet,
    agave_precompiles::secp256k1::verify,
    rand0_7::{Rng, thread_rng},
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
    (0..TX_COUNT)
        .map(|_| {
            let mut rng = thread_rng();
            let secp_privkey = libsecp256k1::SecretKey::random(&mut thread_rng());
            let message: Vec<u8> = (0..message_length).map(|_| rng.gen_range(0, 255)).collect();
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

#[bench]
fn bench_secp256k1_len_032(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(32);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}

#[bench]
fn bench_secp256k1_len_256(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(256);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}

#[bench]
fn bench_secp256k1_len_32k(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(32 * 1024);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}

#[bench]
fn bench_secp256k1_len_max(b: &mut Bencher) {
    let required_extra_space = 113_u16; // len for pubkey, sig, and offsets
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(u16::MAX - required_extra_space);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}
