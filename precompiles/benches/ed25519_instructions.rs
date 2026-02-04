#![feature(test)]

extern crate test;
use {
    agave_feature_set::FeatureSet,
    agave_precompiles::ed25519::verify,
    ed25519_dalek::{Signer, SigningKey},
    rand::RngCore,
    solana_ed25519_program::new_ed25519_instruction_with_signature,
    solana_instruction::Instruction,
    test::Bencher,
};

// 5K instructions should be enough for benching loop
const IX_COUNT: u16 = 5120;

fn generate_signing_key() -> SigningKey {
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

// prepare a bunch of unique txs
fn create_test_instructions(message_length: u16) -> Vec<Instruction> {
    use rand::Rng;
    (0..IX_COUNT)
        .map(|_| {
            let privkey = generate_signing_key();
            let message: Vec<u8> = (0..message_length)
                .map(|_| rand::rng().random_range(0..255))
                .collect();
            let signature = privkey.sign(&message).to_bytes();
            let pubkey = privkey.verifying_key().to_bytes();
            new_ed25519_instruction_with_signature(&message, &signature, &pubkey)
        })
        .collect()
}

#[bench]
fn bench_ed25519_len_032(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(32);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}

#[bench]
fn bench_ed25519_len_128(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(128);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}

#[bench]
fn bench_ed25519_len_32k(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(32 * 1024);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}

#[bench]
fn bench_ed25519_len_max(b: &mut Bencher) {
    let required_extra_space = 113_u16; // len for pubkey, sig, and offsets
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(u16::MAX - required_extra_space);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}
