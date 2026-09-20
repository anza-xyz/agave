use {
    agave_bls_cert_verify::cert_verify::{
        aggregate_pubkeys, collect_pubkeys, test_create_base2_unverified_certificate,
        test_create_base3_unverified_certificate, verify_certificate,
    },
    agave_votor_messages::certificate::CertificateType,
    bitvec::vec::BitVec,
    criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main},
    rand::Rng,
    solana_bls_signatures::{
        keypair::Keypair as BlsKeypair,
        pubkey::{PopVerified, PubkeyAffine as BlsPubkeyAffine},
    },
    std::num::NonZero,
};

// Creates random BLS keypairs for bench tests
fn create_bls_keypairs(num_signers: usize) -> Vec<BlsKeypair> {
    (0..num_signers).map(|_| BlsKeypair::new()).collect()
}

#[allow(clippy::arithmetic_side_effects)]
fn bench_verify_cert(c: &mut Criterion) {
    let mut group = c.benchmark_group("BLS Cert Verify");
    let validator_sizes = [500, 1000, 1500, 2000];
    const TEST_STAKE: u64 = 30; // assume each validator has stake 30 (arbitrary number)

    // Baseline: Single BLS Signature Verification
    // This is the absolute lower bound for cert verification.
    group.bench_function("Single_Signature_Verify_Baseline", |b| {
        let keypair = BlsKeypair::new();
        let payload = b"test_payload";
        let signature = keypair.sign(payload);
        b.iter(|| {
            keypair
                .verify(&signature, payload)
                .expect("Verification failed");
        })
    });

    for &size in &validator_sizes {
        let keypairs = create_bls_keypairs(size);
        let shred_version = rand::rng().random();

        // Pre-calculate public keys to simulate efficient Bank lookup
        let pubkeys: Vec<PopVerified<BlsPubkeyAffine>> =
            keypairs.iter().map(|kp| kp.public).collect();
        let pubkeys_ref = &pubkeys;

        // Base2 Setup
        // Assume 2/3rds of validators sign
        let num_signers_base2 = (size * 2) / 3;
        let slot = 100;
        let cert_type = CertificateType::new_unique_notar(slot);
        let cert_base2 = test_create_base2_unverified_certificate(
            &keypairs,
            shred_version,
            cert_type,
            &(0..num_signers_base2).collect::<Vec<_>>(),
        );

        // Collect pubkeys
        let mut ranks_bitvec = BitVec::<u8>::with_capacity(size);
        ranks_bitvec.resize(size, false);
        for i in 0..num_signers_base2 {
            ranks_bitvec.set(i, true);
        }
        group.bench_with_input(
            BenchmarkId::new("Component_Collect_Pubkeys", size),
            &size,
            |b, &_| {
                b.iter(|| {
                    collect_pubkeys(&ranks_bitvec, |rank| pubkeys_ref.get(rank).cloned()).unwrap()
                })
            },
        );

        // Pubkey Aggregation
        let pre_collected_pubkeys =
            collect_pubkeys(&ranks_bitvec, |rank| pubkeys_ref.get(rank).cloned()).unwrap();
        group.bench_with_input(
            BenchmarkId::new("Component_Aggregate_Pubkeys", size),
            &size,
            |b, &_| b.iter(|| aggregate_pubkeys(&pre_collected_pubkeys).unwrap()),
        );

        group.bench_with_input(
            BenchmarkId::new("Base2_Notarize", size),
            &size,
            |b, &total_validators| {
                let total_stake = NonZero::new(TEST_STAKE * total_validators as u64).unwrap();
                b.iter_batched(
                    || cert_base2.clone(),
                    |cert_base2| {
                        // The rank_map closure simulates the Bank lookup.
                        // It adds stake (we use 1000 per validator) and returns the pubkey.
                        verify_certificate(cert_base2, total_validators, total_stake, |rank| {
                            pubkeys_ref
                                .get(rank)
                                .map(|bls_pubkey| (NonZero::new(TEST_STAKE).unwrap(), *bls_pubkey))
                        })
                        .unwrap();
                    },
                    BatchSize::SmallInput,
                )
            },
        );

        // Base3 Setup: Split vote
        // 40% sign Notarize, 30% sign Fallback (Total 70%)
        let num_notarize = (size * 40) / 100;
        let num_fallback = (size * 30) / 100;
        let slot = 100;
        let cert_type = CertificateType::new_unique_notar_fallback(slot);
        let cert_base3 = test_create_base3_unverified_certificate(
            &keypairs,
            shred_version,
            cert_type,
            &(0..num_notarize).collect::<Vec<_>>(),
            &(num_notarize..num_notarize.saturating_add(num_fallback)).collect::<Vec<_>>(),
        );

        group.bench_with_input(
            BenchmarkId::new("Base3_NotarizeFallback", size),
            &size,
            |b, &total_validators| {
                let total_stake = NonZero::new(TEST_STAKE * total_validators as u64).unwrap();
                b.iter_batched(
                    || cert_base3.clone(),
                    |cert_base3| {
                        verify_certificate(cert_base3, total_validators, total_stake, |rank| {
                            pubkeys_ref
                                .get(rank)
                                .map(|bls_pubkey| (NonZero::new(TEST_STAKE).unwrap(), *bls_pubkey))
                        })
                        .unwrap();
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_verify_cert);
criterion_main!(benches);
