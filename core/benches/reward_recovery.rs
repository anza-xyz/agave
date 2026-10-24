//! Costs of reward recovery for the leader that builds a source slot's reward certificates.
//!
//! The direct votes of the last half of the validators arrive, and a certificate holds the first
//! three quarters, so recovery can publish every validator once a background job has verified the
//! certificate extended with the raw signatures outside it. Keys, signatures and inputs are
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
        Keypair as BlsKeypair, PubkeyCompressed, Scalar, Signature, SignatureProjective,
    },
    solana_clock::Slot,
    solana_core::reward_certs::CertsBuilder,
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::Keypair,
    solana_net_utils::SocketAddrSpace,
    solana_runtime::{
        bank::{Bank, MAX_ALPENGLOW_VOTE_ACCOUNTS, SlotLeader},
        bank_forks::BankForks,
        genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
    },
    solana_signer::Signer,
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
    cluster_info: Arc<ClusterInfo>,
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
        let keypair = Arc::new(Keypair::new());
        let mut contact = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        contact.set_shred_version(SHRED_VERSION);
        let cluster_info = Arc::new(ClusterInfo::new(
            contact,
            keypair,
            SocketAddrSpace::Unspecified,
        ));
        Self {
            bank,
            _bank_forks: bank_forks,
            cluster_info,
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

    fn direct_votes(&self, ranks: Range<usize>) -> RewardInput {
        let rank_map = self.bank.get_rank_map(SOURCE_SLOT).unwrap();
        RewardInput::External(vec![VoteAggregate::new_from_verified_votes(
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
        )])
    }

    fn raw_signatures(&self, ranks: Range<usize>) -> RewardInput {
        RewardInput::VoteSignatures {
            vote: self.vote,
            signatures: self.signatures[ranks].to_vec(),
        }
    }

    /// Signatures of `ranks` in which each pair of ranks in `pairs` submits components that
    /// cancel, so their batch still verifies although neither component does.
    fn cancelling_signatures(
        &self,
        ranks: Range<usize>,
        pairs: &[(usize, usize)],
    ) -> Vec<(u16, Signature)> {
        let mut signatures = self.signatures[ranks.clone()].to_vec();
        let delta: Signature = BlsKeypair::new().sign(b"cancelling components").into();
        for &(first, second) in pairs {
            for (rank, weight) in [(first, Scalar::ONE), (second, -Scalar::ONE)] {
                let signature = &mut signatures[rank - ranks.start].1;
                *signature = SignatureProjective::aggregate_with_weights(
                    [&*signature, &delta].into_iter(),
                    [Scalar::ONE, weight].iter(),
                )
                .unwrap()
                .into();
            }
        }
        signatures
    }

    /// A builder that holds `direct` votes and certificates for `seeds`, before raw signatures.
    fn builder(
        &self,
        enable_reward_recovery: bool,
        direct: Range<usize>,
        seeds: &[Range<usize>],
    ) -> CertsBuilder {
        let mut builder = CertsBuilder::new(self.cluster_info.clone(), enable_reward_recovery);
        builder.handle_input(&self.bank, self.direct_votes(direct));
        builder.handle_input(
            &self.bank,
            RewardInput::Certificates(
                seeds
                    .iter()
                    .map(|ranks| self.certificate(ranks.clone()))
                    .collect(),
            ),
        );
        builder
    }
}

/// Polls until no recovery job is in flight or needed.
fn wait_until_idle(builder: &mut CertsBuilder) {
    while builder.poll_interval() != Duration::from_secs(1) {
        builder.poll_recovery();
        std::thread::yield_now();
    }
}

fn build(builder: &mut CertsBuilder) -> usize {
    builder.build_certs(BUILD_SLOT).unwrap().validators.len()
}

/// Times building the reward certificates of builders that `setup` prepares, which must pay
/// `expected` validators. Preparing, checking and dropping each builder are not timed.
fn bench_build(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name: &str,
    n: usize,
    expected: usize,
    mut setup: impl FnMut() -> CertsBuilder,
) {
    group.bench_function(BenchmarkId::new(name, n), |bencher| {
        bencher.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let mut builder = setup();
                let started = Instant::now();
                let response = builder.build_certs(BUILD_SLOT);
                elapsed += started.elapsed();
                assert_eq!(response.unwrap().validators.len(), expected);
            }
            elapsed
        });
    });
}

/// Times raw signatures for `direct` from arrival until the job that extends `seeds` with them is
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
                let mut builder = fixture.builder(true, direct.clone(), seeds);
                // Keep this metric about extending already prepared seeds. Certificate admission
                // now performs its own replay checks on the same bounded worker.
                wait_until_idle(&mut builder);
                let input = fixture.raw_signatures(direct.clone());
                let started = Instant::now();
                builder.handle_input(&fixture.bank, input);
                wait_until_idle(&mut builder);
                elapsed += started.elapsed();
                assert!(build(&mut builder) > direct.len(), "nothing was recovered");
            }
            elapsed
        });
    });
}

/// Times the worker on the longest job once raw signatures for `direct` arrive. Two colluding
/// pairs submit components that cancel within their batch. The seed covers one member of each
/// pair, so the extension holds the other two invalid components, one in each half. The first job
/// then makes the most checks a job may make, bisecting the extension. Every eviction schedules
/// another job only after the check interval, so the time until recovery is idle would mostly
/// measure that pacing.
fn bench_bisection(c: &mut Criterion, fixture: &Fixture, direct: Range<usize>, seed: Range<usize>) {
    let n = fixture.validators();
    let outside = seed.end..n;
    let quarter = outside.len() / 4;
    let pairs = [
        (direct.start, outside.start + quarter),
        (direct.start + 1, outside.start + 3 * quarter),
    ];
    assert!(pairs.iter().all(|(covered, _)| seed.contains(covered)));
    let signatures = fixture.cancelling_signatures(direct.clone(), &pairs);
    c.bench_function(
        &format!("reward_recovery/bisection_worker/{n}"),
        |bencher| {
            bencher.iter_custom(|iterations| {
                let mut worker = Duration::ZERO;
                for _ in 0..iterations {
                    let mut builder =
                        fixture.builder(true, direct.clone(), std::slice::from_ref(&seed));
                    wait_until_idle(&mut builder);
                    builder.handle_input(
                        &fixture.bank,
                        RewardInput::VoteSignatures {
                            vote: fixture.vote,
                            signatures: signatures.clone(),
                        },
                    );
                    wait_until_idle(&mut builder);
                    worker += Duration::from_micros(builder.longest_recovery_job_us().unwrap());
                    // Both components that reached the extension are evicted, and only their signers
                    // are left out.
                    assert_eq!(build(&mut builder), n - pairs.len());
                }
                worker
            });
        },
    );
}

fn bench_reward_recovery(c: &mut Criterion) {
    for n in [1024, MAX_ALPENGLOW_VOTE_ACCOUNTS] {
        let fixture = Fixture::new(n);
        let direct = n / 2..n;
        let seed = 0..n * 3 / 4;

        let mut group = c.benchmark_group("reward_recovery");
        group.sample_size(20);
        // Publishing the direct votes alone, as without recovery.
        bench_build(&mut group, "direct_build", n, n / 2, || {
            fixture.builder(false, direct.clone(), std::slice::from_ref(&seed))
        });
        // Selecting and wrapping prepared, replay-checked evidence; builds perform no pairings.
        bench_build(&mut group, "recovered_build", n, n, || {
            let mut builder = fixture.builder(true, direct.clone(), std::slice::from_ref(&seed));
            builder.handle_input(&fixture.bank, fixture.raw_signatures(direct.clone()));
            wait_until_idle(&mut builder);
            builder
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
            bench_bisection(c, &fixture, direct, seed);
        }
    }
}

criterion_group!(benches, bench_reward_recovery);
criterion_main!(benches);
