//! Costs of reward recovery for the leader that builds a source slot's reward certificates.
//!
//! The direct votes of the last half of the validators arrive, and a certificate holds the first
//! three quarters, so recovery can publish every validator once a background job has verified the
//! certificate extended with disjoint vote aggregates. Keys, signatures and inputs are
//! prepared outside the measured regions, and every measured output's validator count is checked
//! afterwards. The production service load probe additionally checks signatures and metadata.

#![allow(clippy::arithmetic_side_effects)]

use {
    agave_bls_sigverify::{rewards::RewardInput, sig_verified_messages::VoteAggregate},
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        reward_certificate::NUM_SLOTS_FOR_REWARD,
        vote::Vote,
        wire::{VotePayloadToSign, get_vote_payload_to_sign},
    },
    bitvec::vec::BitVec,
    criterion::{
        BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main,
        measurement::WallTime,
    },
    solana_bls_signatures::{
        Keypair as BlsKeypair, PubkeyCompressed, Signature, SignatureProjective,
    },
    solana_clock::Slot,
    solana_core::reward_certs::{CertsBuilder, Recovery, RewardRespSucc},
    solana_epoch_schedule::EpochSchedule,
    solana_runtime::{
        bank::{Bank, MAX_ALPENGLOW_VOTE_ACCOUNTS, SlotLeader},
        bank_forks::BankForks,
        genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
    },
    solana_signer_store::encode_base2,
    std::{
        collections::HashMap,
        ops::Range,
        sync::{Arc, RwLock},
        time::{Duration, Instant},
    },
};

const SHRED_VERSION: u16 = 7;
const SOURCE_SLOT: Slot = 100;
const BUILD_SLOT: Slot = SOURCE_SLOT + NUM_SLOTS_FOR_REWARD;

struct Fixture {
    bank: Arc<Bank>,
    _bank_forks: Arc<RwLock<BankForks>>,
    vote: Vote,
    /// Every validator's signature of `vote`, by rank.
    signatures: Vec<(u16, Signature)>,
}

impl Fixture {
    fn new(validators: usize) -> Self {
        let validator_keys: Vec<_> = (0..validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect();
        let bls_keys: HashMap<_, _> = validator_keys
            .iter()
            .map(|keys| {
                (
                    PubkeyCompressed::from(keys.bls_keypair.public.into_inner()),
                    keys.bls_keypair.clone(),
                )
            })
            .collect();
        let mut genesis = create_genesis_config_with_vote_accounts(
            1_000_000_000,
            &validator_keys,
            // Unequal stakes make the rank order deterministic.
            (0..validators)
                .map(|rank| (validators - rank) as u64)
                .collect(),
        )
        .genesis_config;
        genesis.epoch_schedule = EpochSchedule::without_warmup();
        let (bank, bank_forks) = Bank::new_for_tests(&genesis).wrap_with_bank_forks_for_tests();
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            SlotLeader::default(),
            SOURCE_SLOT,
        );
        let rank_map = bank.get_rank_map(SOURCE_SLOT).unwrap();
        assert_eq!(rank_map.len(), validators);
        let vote = Vote::new_skip_vote(SOURCE_SLOT);
        let payload = get_vote_payload_to_sign(vote, SHRED_VERSION);
        let signatures = rank_map
            .iter()
            .enumerate()
            .map(|(rank, (_, entry))| {
                let key: &BlsKeypair = &bls_keys[&PubkeyCompressed::from(*entry.bls_pubkey)];
                (rank as u16, key.sign(&payload).into())
            })
            .collect();
        Self {
            bank,
            _bank_forks: bank_forks,
            vote,
            signatures,
        }
    }

    fn validators(&self) -> usize {
        self.signatures.len()
    }

    fn certificate(&self, ranks: Range<usize>) -> Certificate {
        let mut bitmap = BitVec::<u8>::repeat(false, self.validators());
        bitmap[ranks.clone()].fill(true);
        Certificate {
            cert_type: CertificateType::Skip(SOURCE_SLOT),
            signature: SignatureProjective::aggregate(
                self.signatures[ranks]
                    .iter()
                    .map(|(_, signature)| signature),
            )
            .unwrap()
            .into(),
            bitmap: encode_base2(&bitmap).unwrap(),
        }
    }

    fn vote_aggregate(&self, ranks: Range<usize>) -> VoteAggregate {
        let rank_map = self.bank.get_rank_map(SOURCE_SLOT).unwrap();
        VoteAggregate::new_from_verified_votes(
            self.validators(),
            VotePayloadToSign::new_from_vote(self.vote, SHRED_VERSION),
            ranks.clone().map(|rank| {
                let stake = rank_map.get_pubkey_stake_entry(rank).unwrap().stake;
                (rank as u16, stake)
            }),
            SignatureProjective::aggregate(
                self.signatures[ranks]
                    .iter()
                    .map(|(_, signature)| signature),
            )
            .unwrap(),
        )
    }

    fn direct_votes(&self, ranks: Range<usize>) -> RewardInput {
        let batch_size = (self.validators() / 4).max(1);
        RewardInput::External(
            (ranks.start..ranks.end)
                .step_by(batch_size)
                .map(|start| self.vote_aggregate(start..(start + batch_size).min(ranks.end)))
                .collect(),
        )
    }

    /// Prepares the direct builder and recovery engine independently for computation benchmarks.
    /// Controller routing and deadlines are exercised by the production service load probe.
    fn prepare(&self, direct: Range<usize>, seeds: &[Range<usize>]) -> (CertsBuilder, Recovery) {
        let mut builder = CertsBuilder::new();
        let mut recovery = Recovery::new();
        let rank_map = self.bank.get_rank_map(SOURCE_SLOT).unwrap();
        let input = self.direct_votes(direct);
        builder.handle_input(&self.bank, &input);
        let RewardInput::External(aggregates) = input else {
            unreachable!();
        };
        for aggregate in aggregates {
            recovery.add_aggregate(aggregate, rank_map);
        }
        for ranks in seeds {
            recovery.add_certificate(self.certificate(ranks.clone()), rank_map);
        }
        recovery.poll(SHRED_VERSION);
        (builder, recovery)
    }
}

/// Polls until no recovery job is in flight or needed.
fn wait_until_idle(recovery: &mut Recovery) {
    while recovery.poll_interval() != Duration::from_secs(1) {
        recovery.poll(SHRED_VERSION);
        std::thread::yield_now();
    }
}

fn build(builder: &mut CertsBuilder, recovery: &mut Recovery) -> RewardRespSucc {
    let baseline = builder.build_certs(BUILD_SLOT).unwrap();
    recovery.finish(SOURCE_SLOT, SHRED_VERSION, baseline)
}

/// Times building the reward certificates of builders that `setup` prepares, which must pay
/// `expected` validators. Preparing, checking and dropping each builder are not timed.
fn bench_build(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name: &str,
    n: usize,
    expected: usize,
    mut setup: impl FnMut() -> (CertsBuilder, Recovery),
) {
    group.bench_function(BenchmarkId::new(name, n), |bencher| {
        bencher.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let (mut builder, mut recovery) = setup();
                let started = Instant::now();
                let response = build(&mut builder, &mut recovery);
                elapsed += started.elapsed();
                assert_eq!(response.validators.len(), expected);
            }
            elapsed
        });
    });
}

/// Times vote aggregates for `direct` from arrival until the job that extends `seeds` with them is
/// collected.
fn bench_job(
    c: &mut Criterion,
    name: &str,
    fixture: &Fixture,
    direct: Range<usize>,
    seeds: &[Range<usize>],
) {
    let n = fixture.validators();
    c.bench_function(&format!("reward_recovery/{name}/{n}"), |bencher| {
        bencher.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let (mut builder, mut recovery) = fixture.prepare(0..0, seeds);
                // Keep this metric about extending already prepared seeds. Certificate admission
                // now performs its own replay checks on the same bounded worker.
                wait_until_idle(&mut recovery);
                let input = fixture.direct_votes(direct.clone());
                let RewardInput::External(aggregates) = input else {
                    unreachable!();
                };
                let rank_map = fixture.bank.get_rank_map(SOURCE_SLOT).unwrap();
                let started = Instant::now();
                for aggregate in aggregates {
                    recovery.add_aggregate(aggregate, rank_map);
                }
                wait_until_idle(&mut recovery);
                elapsed += started.elapsed();
                assert!(
                    build(&mut builder, &mut recovery).validators.len() > direct.len(),
                    "nothing was recovered"
                );
            }
            elapsed
        });
    });
}

fn bench_reward_recovery(c: &mut Criterion) {
    for n in [1024, MAX_ALPENGLOW_VOTE_ACCOUNTS] {
        let fixture = Fixture::new(n);
        let direct = n / 2..n;
        let seed = 0..n * 3 / 4;

        let mut group = c.benchmark_group("reward_recovery");
        group.sample_size(20);
        // Publishing direct votes when no certificate evidence arrived.
        bench_build(&mut group, "direct_build", n, n / 2, || {
            fixture.prepare(direct.clone(), &[])
        });
        // Selecting and wrapping prepared, replay-checked evidence; builds perform no pairings.
        bench_build(&mut group, "recovered_build", n, n, || {
            let (builder, mut recovery) =
                fixture.prepare(direct.clone(), std::slice::from_ref(&seed));
            wait_until_idle(&mut recovery);
            (builder, recovery)
        });
        group.finish();

        bench_job(
            c,
            "job",
            &fixture,
            direct.clone(),
            std::slice::from_ref(&seed),
        );
        if n == MAX_ALPENGLOW_VOTE_ACCOUNTS {
            // Four seeds whose extensions are incomparable, so the job checks each of them.
            let fifth = n / 5;
            let seeds: Vec<_> = (1..5)
                .map(|group| group * fifth..(group + 1) * fifth)
                .collect();
            bench_job(c, "four_seed_job", &fixture, 0..fifth, &seeds);
        }
    }
}

criterion_group!(benches, bench_reward_recovery);
criterion_main!(benches);
