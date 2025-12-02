#![feature(test)]

extern crate test;
use {
    agave_feature_set::FeatureSet,
    agave_precompiles::ed25519::verify,
    rand_chacha::rand_core::RngCore,
    rand_chacha::{rand_core::SeedableRng, ChaCha20Rng},
    solana_ed25519_program::new_ed25519_instruction_with_signature,
    solana_instruction::Instruction,
    test::Bencher,
};

// 5K instructions should be enough for benching loop
const IX_COUNT: u16 = 5120;

// prepare a bunch of unique txs
fn create_test_instructions(message_length: u16) -> Vec<Instruction> {
    (0..IX_COUNT)
        .map(|_| {
            let mut rng = ChaCha20Rng::from_seed([0u8; 32]);
            let privkey = ed25519_zebra::SigningKey::new(&mut rng);
            let message: Vec<u8> = (0..message_length).map(|_| rng.next_u32() as u8).collect();
            let signature = privkey.sign(&message).to_bytes();
            let pubkey = privkey.verification_key().try_into().unwrap();
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
