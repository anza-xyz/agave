#![allow(clippy::arithmetic_side_effects)]
use {
    criterion::{criterion_group, criterion_main, Criterion},
    curve25519_dalek::scalar::Scalar,
    solana_zk_sdk::{
        encryption::{
            elgamal::ElGamalKeypair,
            grouped_elgamal::GroupedElGamal,
            pedersen::{Pedersen, PedersenOpening},
        },
        zk_elgamal_proof_program::proof_data::{
            BatchedGroupedCiphertext2HandlesValidityProofData,
            BatchedGroupedCiphertext3HandlesValidityProofData, BatchedRangeProofU128Data,
            BatchedRangeProofU256Data, BatchedRangeProofU64Data,
            CiphertextCiphertextEqualityProofData, CiphertextCommitmentEqualityProofData,
            GroupedCiphertext2HandlesValidityProofData, GroupedCiphertext3HandlesValidityProofData,
            PercentageWithCapProofData, PubkeyValidityProofData, ZeroCiphertextProofData,
            ZkProofData,
        },
    },
};

fn bench_pubkey_validity(c: &mut Criterion) {
    let keypair = ElGamalKeypair::new_rand();
    let proof_data = PubkeyValidityProofData::new(&keypair).unwrap();

    c.bench_function("pubkey_validity", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_zero_ciphertext(c: &mut Criterion) {
    let keypair = ElGamalKeypair::new_rand();
    let ciphertext = keypair.pubkey().encrypt(0_u64);
    let proof_data = ZeroCiphertextProofData::new(&keypair, &ciphertext).unwrap();

    c.bench_function("zero_ciphertext", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_grouped_ciphertext_2_handles_validity(c: &mut Criterion) {
    let destination_keypair = ElGamalKeypair::new_rand();
    let destination_pubkey = destination_keypair.pubkey();

    let auditor_keypair = ElGamalKeypair::new_rand();
    let auditor_pubkey = auditor_keypair.pubkey();

    let amount: u64 = 55;
    let opening = PedersenOpening::new_rand();
    let grouped_ciphertext =
        GroupedElGamal::encrypt_with([destination_pubkey, auditor_pubkey], amount, &opening);

    let proof_data = GroupedCiphertext2HandlesValidityProofData::new(
        destination_pubkey,
        auditor_pubkey,
        &grouped_ciphertext,
        amount,
        &opening,
    )
    .unwrap();

    c.bench_function("grouped_ciphertext_2_handles_validity", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_grouped_ciphertext_3_handles_validity(c: &mut Criterion) {
    let source_keypair = ElGamalKeypair::new_rand();
    let source_pubkey = source_keypair.pubkey();

    let destination_keypair = ElGamalKeypair::new_rand();
    let destination_pubkey = destination_keypair.pubkey();

    let auditor_keypair = ElGamalKeypair::new_rand();
    let auditor_pubkey = auditor_keypair.pubkey();

    let amount: u64 = 55;
    let opening = PedersenOpening::new_rand();
    let grouped_ciphertext = GroupedElGamal::encrypt_with(
        [source_pubkey, destination_pubkey, auditor_pubkey],
        amount,
        &opening,
    );

    let proof_data = GroupedCiphertext3HandlesValidityProofData::new(
        source_pubkey,
        destination_pubkey,
        auditor_pubkey,
        &grouped_ciphertext,
        amount,
        &opening,
    )
    .unwrap();

    c.bench_function("grouped_ciphertext_3_handles_validity", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_ciphertext_commitment_equality(c: &mut Criterion) {
    let keypair = ElGamalKeypair::new_rand();
    let amount: u64 = 55;
    let ciphertext = keypair.pubkey().encrypt(amount);
    let (commitment, opening) = Pedersen::new(amount);

    let proof_data = CiphertextCommitmentEqualityProofData::new(
        &keypair,
        &ciphertext,
        &commitment,
        &opening,
        amount,
    )
    .unwrap();

    c.bench_function("ciphertext_commitment_equality", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_ciphertext_ciphertext_equality(c: &mut Criterion) {
    let source_keypair = ElGamalKeypair::new_rand();
    let destination_keypair = ElGamalKeypair::new_rand();

    let amount: u64 = 0;
    let source_ciphertext = source_keypair.pubkey().encrypt(amount);

    let destination_opening = PedersenOpening::new_rand();
    let destination_ciphertext = destination_keypair
        .pubkey()
        .encrypt_with(amount, &destination_opening);

    let proof_data = CiphertextCiphertextEqualityProofData::new(
        &source_keypair,
        destination_keypair.pubkey(),
        &source_ciphertext,
        &destination_ciphertext,
        &destination_opening,
        amount,
    )
    .unwrap();

    c.bench_function("ciphertext_ciphertext_equality", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_batched_grouped_ciphertext_2_handles_validity(c: &mut Criterion) {
    let destination_keypair = ElGamalKeypair::new_rand();
    let destination_pubkey = destination_keypair.pubkey();

    let auditor_keypair = ElGamalKeypair::new_rand();
    let auditor_pubkey = auditor_keypair.pubkey();

    let amount_lo: u64 = 11;
    let amount_hi: u64 = 22;

    let opening_lo = PedersenOpening::new_rand();
    let opening_hi = PedersenOpening::new_rand();

    let grouped_ciphertext_lo =
        GroupedElGamal::encrypt_with([destination_pubkey, auditor_pubkey], amount_lo, &opening_lo);

    let grouped_ciphertext_hi =
        GroupedElGamal::encrypt_with([destination_pubkey, auditor_pubkey], amount_hi, &opening_hi);

    let proof_data = BatchedGroupedCiphertext2HandlesValidityProofData::new(
        destination_pubkey,
        auditor_pubkey,
        &grouped_ciphertext_lo,
        &grouped_ciphertext_hi,
        amount_lo,
        amount_hi,
        &opening_lo,
        &opening_hi,
    )
    .unwrap();

    c.bench_function("batched_grouped_ciphertext_validity", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_batched_grouped_ciphertext_3_handles_validity(c: &mut Criterion) {
    let source_keypair = ElGamalKeypair::new_rand();
    let source_pubkey = source_keypair.pubkey();

    let destination_keypair = ElGamalKeypair::new_rand();
    let destination_pubkey = destination_keypair.pubkey();

    let auditor_keypair = ElGamalKeypair::new_rand();
    let auditor_pubkey = auditor_keypair.pubkey();

    let amount_lo: u64 = 11;
    let amount_hi: u64 = 22;

    let opening_lo = PedersenOpening::new_rand();
    let opening_hi = PedersenOpening::new_rand();

    let grouped_ciphertext_lo = GroupedElGamal::encrypt_with(
        [source_pubkey, destination_pubkey, auditor_pubkey],
        amount_lo,
        &opening_lo,
    );

    let grouped_ciphertext_hi = GroupedElGamal::encrypt_with(
        [source_pubkey, destination_pubkey, auditor_pubkey],
        amount_hi,
        &opening_hi,
    );

    let proof_data = BatchedGroupedCiphertext3HandlesValidityProofData::new(
        source_pubkey,
        destination_pubkey,
        auditor_pubkey,
        &grouped_ciphertext_lo,
        &grouped_ciphertext_hi,
        amount_lo,
        amount_hi,
        &opening_lo,
        &opening_hi,
    )
    .unwrap();

    c.bench_function("batched_grouped_ciphertext_3_handles_validity", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

#[allow(clippy::op_ref)]
fn bench_percentage_with_cap(c: &mut Criterion) {
    let transfer_amount: u64 = 1;
    let max_fee: u64 = 3;

    let fee_rate: u16 = 400;
    let fee_amount: u64 = 1;
    let delta_fee: u64 = 9600;

    let (transfer_commitment, transfer_opening) = Pedersen::new(transfer_amount);
    let (fee_commitment, fee_opening) = Pedersen::new(fee_amount);

    let scalar_rate = Scalar::from(fee_rate);
    let delta_commitment =
        &fee_commitment * Scalar::from(10_000_u64) - &transfer_commitment * &scalar_rate;
    let delta_opening = &fee_opening * &Scalar::from(10_000_u64) - &transfer_opening * &scalar_rate;

    let (claimed_commitment, claimed_opening) = Pedersen::new(delta_fee);

    let proof_data = PercentageWithCapProofData::new(
        &fee_commitment,
        &fee_opening,
        fee_amount,
        &delta_commitment,
        &delta_opening,
        delta_fee,
        &claimed_commitment,
        &claimed_opening,
        max_fee,
    )
    .unwrap();

    c.bench_function("percentage_with_cap", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_batched_range_proof_u64(c: &mut Criterion) {
    let amount_1 = 255_u64;
    let amount_2 = 77_u64;
    let amount_3 = 99_u64;
    let amount_4 = 99_u64;
    let amount_5 = 11_u64;
    let amount_6 = 33_u64;
    let amount_7 = 99_u64;
    let amount_8 = 99_u64;

    let (commitment_1, opening_1) = Pedersen::new(amount_1);
    let (commitment_2, opening_2) = Pedersen::new(amount_2);
    let (commitment_3, opening_3) = Pedersen::new(amount_3);
    let (commitment_4, opening_4) = Pedersen::new(amount_4);
    let (commitment_5, opening_5) = Pedersen::new(amount_5);
    let (commitment_6, opening_6) = Pedersen::new(amount_6);
    let (commitment_7, opening_7) = Pedersen::new(amount_7);
    let (commitment_8, opening_8) = Pedersen::new(amount_8);

    let proof_data = BatchedRangeProofU64Data::new(
        vec![
            &commitment_1,
            &commitment_2,
            &commitment_3,
            &commitment_4,
            &commitment_5,
            &commitment_6,
            &commitment_7,
            &commitment_8,
        ],
        vec![
            amount_1, amount_2, amount_3, amount_4, amount_5, amount_6, amount_7, amount_8,
        ],
        vec![8, 8, 8, 8, 8, 8, 8, 8],
        vec![
            &opening_1, &opening_2, &opening_3, &opening_4, &opening_5, &opening_6, &opening_7,
            &opening_8,
        ],
    )
    .unwrap();

    c.bench_function("batched_range_proof_u64", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_batched_range_proof_u128(c: &mut Criterion) {
    let amount_1 = 65535_u64;
    let amount_2 = 77_u64;
    let amount_3 = 99_u64;
    let amount_4 = 99_u64;
    let amount_5 = 11_u64;
    let amount_6 = 33_u64;
    let amount_7 = 99_u64;
    let amount_8 = 99_u64;

    let (commitment_1, opening_1) = Pedersen::new(amount_1);
    let (commitment_2, opening_2) = Pedersen::new(amount_2);
    let (commitment_3, opening_3) = Pedersen::new(amount_3);
    let (commitment_4, opening_4) = Pedersen::new(amount_4);
    let (commitment_5, opening_5) = Pedersen::new(amount_5);
    let (commitment_6, opening_6) = Pedersen::new(amount_6);
    let (commitment_7, opening_7) = Pedersen::new(amount_7);
    let (commitment_8, opening_8) = Pedersen::new(amount_8);

    let proof_data = BatchedRangeProofU128Data::new(
        vec![
            &commitment_1,
            &commitment_2,
            &commitment_3,
            &commitment_4,
            &commitment_5,
            &commitment_6,
            &commitment_7,
            &commitment_8,
        ],
        vec![
            amount_1, amount_2, amount_3, amount_4, amount_5, amount_6, amount_7, amount_8,
        ],
        vec![16, 16, 16, 16, 16, 16, 16, 16],
        vec![
            &opening_1, &opening_2, &opening_3, &opening_4, &opening_5, &opening_6, &opening_7,
            &opening_8,
        ],
    )
    .unwrap();

    c.bench_function("batched_range_proof_u128", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

fn bench_batched_range_proof_u256(c: &mut Criterion) {
    let amount_1 = 4294967295_u64;
    let amount_2 = 77_u64;
    let amount_3 = 99_u64;
    let amount_4 = 99_u64;
    let amount_5 = 11_u64;
    let amount_6 = 33_u64;
    let amount_7 = 99_u64;
    let amount_8 = 99_u64;

    let (commitment_1, opening_1) = Pedersen::new(amount_1);
    let (commitment_2, opening_2) = Pedersen::new(amount_2);
    let (commitment_3, opening_3) = Pedersen::new(amount_3);
    let (commitment_4, opening_4) = Pedersen::new(amount_4);
    let (commitment_5, opening_5) = Pedersen::new(amount_5);
    let (commitment_6, opening_6) = Pedersen::new(amount_6);
    let (commitment_7, opening_7) = Pedersen::new(amount_7);
    let (commitment_8, opening_8) = Pedersen::new(amount_8);

    let proof_data = BatchedRangeProofU256Data::new(
        vec![
            &commitment_1,
            &commitment_2,
            &commitment_3,
            &commitment_4,
            &commitment_5,
            &commitment_6,
            &commitment_7,
            &commitment_8,
        ],
        vec![
            amount_1, amount_2, amount_3, amount_4, amount_5, amount_6, amount_7, amount_8,
        ],
        vec![32, 32, 32, 32, 32, 32, 32, 32],
        vec![
            &opening_1, &opening_2, &opening_3, &opening_4, &opening_5, &opening_6, &opening_7,
            &opening_8,
        ],
    )
    .unwrap();

    c.bench_function("batched_range_proof_u256", |b| {
        b.iter(|| {
            proof_data.verify_proof().unwrap();
        })
    });
}

criterion_group!(
    benches,
    bench_pubkey_validity,
    bench_zero_ciphertext,
    bench_grouped_ciphertext_2_handles_validity,
    bench_grouped_ciphertext_3_handles_validity,
    bench_ciphertext_commitment_equality,
    bench_ciphertext_ciphertext_equality,
    bench_batched_grouped_ciphertext_2_handles_validity,
    bench_batched_grouped_ciphertext_3_handles_validity,
    bench_batched_range_proof_u64,
    bench_batched_range_proof_u128,
    bench_batched_range_proof_u256,
    bench_percentage_with_cap,
);
criterion_main!(benches);
