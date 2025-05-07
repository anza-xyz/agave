#![feature(test)]

extern crate test;
use {
    agave_feature_set::FeatureSet,
    agave_precompiles::secp256r1::verify,
    openssl::{
        ec::{EcGroup, EcKey},
        nid::Nid,
    },
    rand0_7::{thread_rng, Rng},
    solana_instruction::Instruction,
    solana_secp256r1_program::new_secp256r1_instruction,
    test::Bencher,
};

// 5k transactions should be enough for benching loop
const TX_COUNT: u16 = 5120;

// prepare a bunch of unique ixs
fn create_test_instructions(message_length: u16) -> Vec<Instruction> {
    (0..TX_COUNT)
        .map(|_| {
            let mut rng = thread_rng();
            let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
            let secp_privkey = EcKey::generate(&group).unwrap();
            let message: Vec<u8> = (0..message_length).map(|_| rng.gen_range(0, 255)).collect();
            new_secp256r1_instruction(&message, secp_privkey).unwrap()
        })
        .collect()
}

#[bench]
fn bench_secp256r1_len_032(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(32);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}

#[bench]
fn bench_secp256r1_len_256(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(256);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}

#[bench]
fn bench_secp256r1_len_32k(b: &mut Bencher) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(32 * 1024);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}

#[bench]
fn bench_secp256r1_len_max(b: &mut Bencher) {
    let required_extra_space = 113_u16; // len for pubkey, sig, and offsets
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(u16::MAX - required_extra_space);
    let mut ix_iter = ixs.iter().cycle();
    b.iter(|| {
        let instruction = ix_iter.next().unwrap();
        verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
    });
}
