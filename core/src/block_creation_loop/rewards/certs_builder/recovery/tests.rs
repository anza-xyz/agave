use {
    super::{super::CertsBuilder, *},
    crate::block_creation_loop::rewards::{
        msg_types::RewardRequest, reward_certs_service::run_with_builder_for_tests,
    },
    agave_bls_sigverify::{rewards::RewardInput, sig_verified_messages::VoteAggregate},
    agave_votor_messages::{
        certificate::CertificateType,
        consensus_message::{Block, BlockId, VoteMessage},
        reward_certificate::NUM_SLOTS_FOR_REWARD,
    },
    rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom},
    solana_bls_signatures::{
        BLS_SIGNATURE_AFFINE_SIZE, Keypair as BlsKeypair, PubkeyCompressed, Scalar,
    },
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::Keypair,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, SlotLeader},
        bank_forks::BankForks,
        epoch_stakes::VersionedEpochStakes,
        genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
    },
    solana_signer::Signer,
    solana_signer_store::encode_base3,
    std::{
        collections::HashSet,
        sync::{
            RwLock,
            atomic::{AtomicBool, Ordering},
        },
    },
};

const SLOT: Slot = 100;
const SHRED_VERSION: u16 = 7;
const NUM_VALIDATORS: usize = 6;

mod deadline;

struct Fixture {
    slot: Slot,
    bank: Arc<Bank>,
    bank_forks: Arc<RwLock<BankForks>>,
    rank_map: Arc<BLSPubkeyToRankMap>,
    keypairs: Vec<BlsKeypair>,
}

impl Fixture {
    fn new() -> Self {
        Self::new_at(SLOT, EpochSchedule::without_warmup())
    }

    fn new_at(slot: Slot, epoch_schedule: EpochSchedule) -> Self {
        Self::new_with_num_validators(slot, epoch_schedule, NUM_VALIDATORS)
    }

    fn new_with_num_validators(slot: Slot, epoch_schedule: EpochSchedule, count: usize) -> Self {
        Self::new_with_stakes(slot, epoch_schedule, vec![100; count])
    }

    fn new_with_stakes(slot: Slot, epoch_schedule: EpochSchedule, stakes: Vec<u64>) -> Self {
        let count = stakes.len();
        let validator_keypairs: Vec<_> = (0..count)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect();
        let keypairs: HashMap<_, _> = validator_keypairs
            .iter()
            .map(|validator| {
                (
                    PubkeyCompressed::from(validator.bls_keypair.public.into_inner()),
                    validator.bls_keypair.clone(),
                )
            })
            .collect();
        let mut genesis =
            create_genesis_config_with_vote_accounts(1_000_000_000, &validator_keypairs, stakes)
                .genesis_config;
        genesis.epoch_schedule = epoch_schedule;
        let (bank, bank_forks) = Bank::new_for_tests(&genesis).wrap_with_bank_forks_for_tests();
        let bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::default(),
            slot,
        );
        let rank_map = bank.get_rank_map(slot).unwrap().clone();
        let keypairs = (0..rank_map.len())
            .map(|rank| {
                let key = rank_map.get_pubkey_stake_entry(rank).unwrap().bls_pubkey;
                keypairs[&PubkeyCompressed::from(*key)].clone()
            })
            .collect();
        Self {
            slot,
            bank,
            bank_forks,
            rank_map,
            keypairs,
        }
    }

    fn ranks(&self, ranks: &[usize]) -> BitVec<u8> {
        let mut bitmap = BitVec::repeat(false, self.rank_map.len());
        for &rank in ranks {
            bitmap.set(rank, true);
        }
        bitmap
    }

    fn signatures(&self, vote: Vote, ranks: &[usize]) -> Vec<(u16, Signature)> {
        let payload = get_vote_payload_to_sign(vote, SHRED_VERSION);
        ranks
            .iter()
            .map(|&rank| {
                (
                    u16::try_from(rank).unwrap(),
                    self.keypairs[rank].sign(&payload).into(),
                )
            })
            .collect()
    }

    fn aggregate(&self, vote: Vote, ranks: &[usize]) -> Signature {
        SignatureProjective::aggregate(self.signatures(vote, ranks).iter().map(|(_, sig)| sig))
            .unwrap()
            .into()
    }

    fn certificate(&self, vote: Vote, ranks: &[usize]) -> Certificate {
        let cert_type = match vote {
            Vote::Skip(skip) => CertificateType::Skip(skip.slot),
            Vote::Notarize(notar) => CertificateType::Notarize(notar.block),
            _ => panic!("unsupported test certificate"),
        };
        Certificate {
            cert_type,
            signature: self.aggregate(vote, ranks),
            bitmap: encode_base2(&self.ranks(ranks)).unwrap(),
        }
    }

    fn response(&self, vote: Vote, ranks: &[usize]) -> RewardRespSucc {
        let candidate =
            Candidate::from_certificate(self.certificate(vote, ranks), &self.rank_map).unwrap();
        let required = BitVec::repeat(false, self.rank_map.len());
        select_candidates(self.slot, vec![candidate], &self.rank_map, &required).unwrap()
    }

    fn assert_response(
        &self,
        response: &RewardRespSucc,
        expected_skip: Option<&[usize]>,
        expected_notar: Option<(BlockId, &[usize])>,
    ) {
        self.assert_response_at(self.slot, response, expected_skip, expected_notar);
    }

    fn assert_response_at(
        &self,
        slot: Slot,
        response: &RewardRespSucc,
        expected_skip: Option<&[usize]>,
        expected_notar: Option<(BlockId, &[usize])>,
    ) {
        let mut expected_validators = HashSet::new();
        let mut validate =
            |vote, signature: &SignatureCompressed, bitmap: &[u8], expected: &[usize]| {
                let Decoded::Base2(ranks) = decode(bitmap, self.rank_map.len()).unwrap() else {
                    panic!("reward output must use base2");
                };
                assert_eq!(ranks.iter_ones().collect::<Vec<_>>(), expected);
                let payload = get_vote_payload_to_sign(vote, SHRED_VERSION);
                verify_base2(
                    &payload,
                    signature,
                    bitmap,
                    self.rank_map.len(),
                    |rank| {
                        self.rank_map
                            .get_pubkey_stake_entry(rank)
                            .map(|entry| entry.bls_pubkey)
                    },
                    None,
                )
                .unwrap();
                expected_validators.extend(expected.iter().map(|&rank| {
                    self.rank_map
                        .get_pubkey_stake_entry(rank)
                        .unwrap()
                        .vote_account_pubkey
                }));
            };
        match (&response.skip, expected_skip) {
            (Some(skip), Some(ranks)) => {
                assert_eq!(skip.slot, slot);
                validate(
                    Vote::new_skip_vote(slot),
                    &skip.signature,
                    skip.to_bitmap(),
                    ranks,
                );
            }
            (None, None) => (),
            _ => panic!("unexpected skip output"),
        }
        match (&response.notar, expected_notar) {
            (Some(notar), Some((block_id, ranks))) => {
                assert_eq!(notar.slot, slot);
                assert_eq!(notar.block_id, block_id.to_hash());
                validate(
                    Vote::new_notarization_vote(Block { slot, block_id }),
                    &notar.signature,
                    notar.bitmap(),
                    ranks,
                );
            }
            (None, None) => (),
            _ => panic!("unexpected notar output"),
        }
        let validators: HashSet<Pubkey> = response.validators.iter().copied().collect();
        assert_eq!(validators, expected_validators);
        assert_eq!(response.validators.len(), validators.len());
    }
}

fn wait_for_check(recovery: &mut Recovery) {
    wait_for_completed_jobs(recovery, 1);
}

fn wait_for_completed_jobs(recovery: &mut Recovery, count: u64) {
    let start = Instant::now();
    while recovery.metrics.completed_jobs < count {
        recovery.poll(SHRED_VERSION);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "recovery worker did not finish"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn wait_until_prepared(recovery: &mut Recovery) {
    wait_until_prepared_with_shred_version(recovery, SHRED_VERSION);
}

fn wait_until_prepared_with_shred_version(recovery: &mut Recovery, shred_version: u16) {
    let start = Instant::now();
    loop {
        recovery.poll(shred_version);
        if !recovery.in_flight
            && !recovery
                .slots
                .values()
                .any(|slot| slot.payloads.values().any(PayloadState::needs_check))
        {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "recovery did not become idle"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn builder() -> CertsBuilder {
    builder_with_recovery(true)
}

fn builder_with_recovery(enable_reward_recovery: bool) -> CertsBuilder {
    let keypair = Arc::new(Keypair::new());
    let mut contact = ContactInfo::new_localhost(&keypair.pubkey(), 0);
    contact.set_shred_version(SHRED_VERSION);
    CertsBuilder::new(
        Arc::new(ClusterInfo::new(
            contact,
            keypair,
            SocketAddrSpace::Unspecified,
        )),
        enable_reward_recovery,
    )
}

#[test]
fn test_service_loop_schedules_and_collects_jobs_without_further_input() {
    let fixture = Fixture::new();
    let sources = [SLOT, SLOT + 1];
    let mut builder = builder();
    let cluster_info = builder.cluster_info.clone();
    // A gated worker holds the first job while the second source's evidence arrives, so only the
    // idle service loop can schedule the second job.
    let (jobs_sender, jobs_receiver) = bounded::<Job>(1);
    let (results_sender, results_receiver) = bounded(1);
    let (finished_sender, finished_receiver) = bounded(sources.len());
    // This sender drops before the builder on panic, so the worker cannot strand Drop::join.
    let (release_sender, release_receiver) = bounded::<()>(0);
    let worker = thread::spawn(move || {
        while let Ok(job) = jobs_receiver.recv() {
            if release_receiver.recv().is_err() {
                return;
            }
            let slot = job.vote.slot();
            let _ = results_sender.send(job.run());
            let _ = finished_sender.send(slot);
        }
    });
    builder.recovery = Some(Recovery::from_worker(jobs_sender, results_receiver, worker));
    let direct = |vote: Vote, rank: usize| {
        VoteAggregate::new_from_verified_vote(
            NUM_VALIDATORS,
            VoteMessage {
                vote,
                signature: fixture.signatures(vote, &[rank])[0].1.try_into().unwrap(),
                rank: u16::try_from(rank).unwrap(),
                stake: fixture.rank_map.get_pubkey_stake_entry(rank).unwrap().stake,
            },
        )
    };
    for slot in sources {
        let vote = Vote::new_skip_vote(slot);
        // The certificate lacks rank 4's direct vote, so only a verified extension may replace
        // the direct baseline.
        for input in [
            RewardInput::External(vec![direct(vote, 3), direct(vote, 4)]),
            RewardInput::VoteSignatures {
                vote,
                signatures: fixture.signatures(vote, &[3, 4]),
            },
            RewardInput::Certificates(vec![fixture.certificate(vote, &[0, 1, 2, 3])]),
        ] {
            builder.handle_input(&fixture.bank, input);
        }
    }
    assert!(builder.recovery_mut().in_flight);

    let exit = Arc::new(AtomicBool::new(false));
    let (_input, input_receiver) = bounded(0);
    let (requests, request_receiver) = bounded(sources.len());
    let sharable_banks = fixture.bank_forks.read().unwrap().sharable_banks();
    let service = thread::spawn({
        let exit = exit.clone();
        move || {
            run_with_builder_for_tests(
                exit,
                cluster_info,
                input_receiver,
                request_receiver,
                sharable_banks,
                builder,
            )
        }
    });
    let release = || {
        release_sender
            .send_timeout((), Duration::from_secs(10))
            .expect("the worker received a job")
    };
    release();
    assert_eq!(
        finished_receiver.recv_timeout(Duration::from_secs(10)),
        Ok(SLOT)
    );
    // No input arrives from here on. Only the idle loop collects the first result and schedules
    // the second job.
    release();
    assert_eq!(
        finished_receiver.recv_timeout(Duration::from_secs(10)),
        Ok(SLOT + 1)
    );
    for slot in sources {
        let (reply_sender, reply_receiver) = bounded(1);
        requests
            .send(RewardRequest {
                bank_slot: slot + NUM_SLOTS_FOR_REWARD,
                reply_sender,
                request_sent: Instant::now(),
            })
            .unwrap();
        let response = reply_receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        let baseline = response.result.unwrap();
        let response = response
            .recovery
            .map(|receiver| receiver.recv_timeout(Duration::from_secs(10)).unwrap())
            .unwrap_or(baseline);
        fixture.assert_response_at(slot, &response, Some(&[0, 1, 2, 3, 4]), None);
    }
    exit.store(true, Ordering::Relaxed);
    service.join().unwrap();
}

#[test]
fn test_disabled_recovery_publishes_only_direct_votes() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let direct = |rank: usize| {
        VoteAggregate::new_from_verified_vote(
            NUM_VALIDATORS,
            VoteMessage {
                vote,
                signature: fixture.signatures(vote, &[rank])[0].1.try_into().unwrap(),
                rank: u16::try_from(rank).unwrap(),
                stake: fixture.rank_map.get_pubkey_stake_entry(rank).unwrap().stake,
            },
        )
    };
    // The certificate covers both direct votes, so recovery would publish it.
    for (enable_reward_recovery, expected) in [(true, &[0, 1, 2, 3][..]), (false, &[0, 1][..])] {
        let mut builder = builder_with_recovery(enable_reward_recovery);
        // Without recovery there is no worker, and nothing to poll for.
        assert_eq!(builder.recovery.is_some(), enable_reward_recovery);
        if !enable_reward_recovery {
            assert_eq!(builder.poll_interval(), Duration::from_secs(1));
        }
        for input in [
            RewardInput::External(vec![direct(0), direct(1)]),
            RewardInput::VoteSignatures {
                vote,
                signatures: fixture.signatures(vote, &[0, 1]),
            },
            RewardInput::Certificates(vec![fixture.certificate(vote, &[0, 1, 2, 3])]),
        ] {
            builder.handle_input(&fixture.bank, input);
        }
        if let Some(recovery) = &mut builder.recovery {
            wait_until_prepared(recovery);
        }
        let response = builder
            .build_certs(SLOT.saturating_add(NUM_SLOTS_FOR_REWARD))
            .unwrap();
        fixture.assert_response(&response, Some(expected), None);
    }
}

#[test]
fn test_certificate_only_recovers_missing_direct_votes() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
    assert_eq!(recovery.metrics.checks, 1);
    assert_eq!(recovery.metrics.recovered_stake, 400);
}

#[test]
fn test_unprepared_seed_cannot_delay_or_replace_the_direct_baseline() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    assert!(recovery.slots[&SLOT].payloads[&vote].needs_check());
    // No job ran. The cutoff consumes only the direct baseline, without checking the new seed.
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[0, 1]));
    fixture.assert_response(&response, Some(&[0, 1]), None);
    assert_eq!(recovery.metrics.checks, 0);
    assert!(!recovery.slots.contains_key(&SLOT));
}

#[test]
fn test_prepared_seed_is_not_reverified_at_build() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.checks, 1);
    assert_eq!(recovery.slots[&SLOT].payloads[&vote].checked_seeds.len(), 1);
    // A gate left at the coordinator cannot be entered by finish: verification lives in Job.
    let (started, started_receiver) = bounded(1);
    let (_release, release_receiver) = bounded(1);
    recovery.verification_gate = Some((started, release_receiver));
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[0, 1]));
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
    assert_eq!(recovery.metrics.checks, 1);
    assert!(started_receiver.try_recv().is_err());
}

#[test]
fn test_replaced_checked_seed_remains_available_while_new_seed_check_is_blocked() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    let original = [0, 1, 2, 3];
    for ranks in [&original[..], &[0, 1, 2, 4], &[0, 1, 2, 5], &[0, 1, 3, 4]] {
        recovery.add_certificate(fixture.certificate(vote, ranks), &fixture.rank_map);
    }
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.checks, MAX_CERTIFICATES_PER_SLOT as u64);
    let completed = recovery.metrics.completed_jobs;
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&vote)
        .unwrap()
        .last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
    let (started, started_receiver) = bounded(1);
    // Drop before recovery on panic, so its worker cannot strand Drop::join at this gate.
    let (release, release_receiver) = bounded(1);
    recovery.verification_gate = Some((started, release_receiver));
    recovery.add_certificate(
        fixture.certificate(vote, &[0, 1, 2, 3, 4]),
        &fixture.rank_map,
    );
    let state = &recovery.slots[&SLOT].payloads[&vote];
    assert_eq!(state.seeds.len(), MAX_CERTIFICATES_PER_SLOT);
    assert!(
        state
            .seeds
            .iter()
            .all(|seed| seed.ranks != fixture.ranks(&original))
    );
    assert_eq!(state.checked_seeds.len(), MAX_CERTIFICATES_PER_SLOT - 1);
    assert_eq!(state.ready.len(), 1);
    assert_eq!(state.ready[0].ranks, fixture.ranks(&original));
    assert!(state.ready.len() <= MAX_READY_CANDIDATES_PER_PAYLOAD);
    recovery.poll(SHRED_VERSION);
    started_receiver
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert!(recovery.in_flight);
    // None of the other retained seeds includes both direct voters. The old checked proof is
    // therefore the only recovery candidate that can preserve this baseline before the cutoff.
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[2, 3]));
    fixture.assert_response(&response, Some(&original), None);
    release.send(()).unwrap();
    wait_for_completed_jobs(&mut recovery, completed + 1);
    assert_eq!(recovery.metrics.late_results, 1);
    assert!(!recovery.slots.contains_key(&SLOT));
}

#[test]
fn test_checked_candidates_require_the_exact_rank_map_and_shred_version() {
    let fixture = Fixture::new();
    let other_fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let candidate = CheckedCandidate::verify(
        Candidate::from_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map)
            .unwrap(),
        &fixture.rank_map,
        SHRED_VERSION,
    )
    .unwrap();
    assert_eq!(fixture.rank_map.len(), other_fixture.rank_map.len());
    for (rank_map, shred_version) in [
        (&other_fixture.rank_map, SHRED_VERSION),
        (&fixture.rank_map, SHRED_VERSION + 1),
    ] {
        let mut recovery = Recovery::new();
        recovery.add_signatures(vote, [], rank_map);
        recovery
            .slots
            .get_mut(&SLOT)
            .unwrap()
            .payloads
            .get_mut(&vote)
            .unwrap()
            .ready
            .push(candidate.clone());
        let response = recovery.finish(SLOT, shred_version, RewardRespSucc::default());
        assert_eq!(response, RewardRespSucc::default());
        assert_eq!(recovery.metrics.checks, 0);
    }
}

#[test]
fn test_changed_shred_version_rechecks_seed_in_the_worker() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.checks, 1);
    wait_until_prepared_with_shred_version(&mut recovery, SHRED_VERSION + 1);
    assert_eq!(recovery.metrics.checks, 2);
    assert_eq!(recovery.metrics.unverified_outputs, 1);
    assert!(matches!(
        recovery.slots[&SLOT].payloads[&vote].checked_seeds[0],
        SeedCheck::Invalid { .. }
    ));
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.checks, 3);
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
}

#[test]
fn test_rejected_seed_cannot_evict_honest_raw_components_or_spin() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut certificate = fixture.certificate(vote, &[0, 1, 2, 3]);
    certificate.signature = fixture.aggregate(vote, &[0]);
    let mut recovery = Recovery::new();
    recovery.add_certificate(certificate, &fixture.rank_map);
    recovery.add_signatures(vote, fixture.signatures(vote, &[4, 5]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.checks, 1);
    assert_eq!(recovery.metrics.unverified_outputs, 1);
    assert_eq!(recovery.metrics.invalid_components, 0);
    let state = &recovery.slots[&SLOT].payloads[&vote];
    assert_eq!(state.signatures.len(), 2);
    assert!(state.ready.is_empty());
    for _ in 0..4 {
        recovery.poll(SHRED_VERSION);
    }
    assert_eq!(recovery.metrics.completed_jobs, 1);
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[4, 5]));
    fixture.assert_response(&response, Some(&[4, 5]), None);
}

#[test]
fn test_seed_preparation_and_bisection_share_the_bounded_job_budget() {
    let fixture = Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), 24);
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    for rank in 0..MAX_CERTIFICATES_PER_SLOT {
        recovery.add_certificate(fixture.certificate(vote, &[rank]), &fixture.rank_map);
    }
    let invalid_ranks: Vec<_> = (MAX_CERTIFICATES_PER_SLOT..24).collect();
    recovery.add_signatures(
        vote,
        fixture.signatures(Vote::new_skip_vote(SLOT + 1), &invalid_ranks),
        &fixture.rank_map,
    );
    wait_for_check(&mut recovery);
    assert!(recovery.metrics.checks <= MAX_CHECKS_PER_JOB);
    assert_eq!(
        recovery.slots[&SLOT].payloads[&vote].checked_seeds.len(),
        MAX_CERTIFICATES_PER_SLOT
    );
    assert!(recovery.metrics.invalid_components > 0);
    wait_until_prepared(&mut recovery);
    assert!(recovery.slots[&SLOT].payloads[&vote].signatures.is_empty());
    assert_eq!(
        recovery.metrics.invalid_components,
        invalid_ranks.len() as u64
    );
    assert!(recovery.metrics.checks <= recovery.metrics.completed_jobs * MAX_CHECKS_PER_JOB);
    let checks = recovery.metrics.checks;
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    assert_eq!(response.validators.len(), 1);
    let skip = response.skip.as_ref().unwrap();
    let Decoded::Base2(ranks) = decode(skip.to_bitmap(), fixture.rank_map.len()).unwrap() else {
        panic!("expected base2 output");
    };
    fixture.assert_response(
        &response,
        Some(&ranks.iter_ones().collect::<Vec<_>>()),
        None,
    );
    assert_eq!(recovery.metrics.checks, checks);
}

#[test]
fn test_certificate_and_outside_direct_votes_merge_in_either_arrival_order() {
    let fixture = Fixture::new();
    let block_id = BlockId::new_unique();
    let vote = Vote::new_notarization_vote(Block {
        slot: SLOT,
        block_id,
    });
    for certificate_first in [false, true] {
        let mut recovery = Recovery::new();
        let cert = fixture.certificate(vote, &[0, 1, 2, 3]);
        // This direct batch straddles the certificate bitmap: ranks 2 and 3 overlap.
        let signatures = fixture.signatures(vote, &[2, 3, 4, 5]);
        if certificate_first {
            recovery.add_certificate(cert, &fixture.rank_map);
            recovery.poll(SHRED_VERSION);
            recovery.add_signatures(vote, signatures, &fixture.rank_map);
        } else {
            recovery.add_signatures(vote, signatures, &fixture.rank_map);
            recovery.poll(SHRED_VERSION);
            recovery.add_certificate(cert, &fixture.rank_map);
        }
        wait_until_prepared(&mut recovery);
        assert_eq!(recovery.metrics.failed_checks, 0);
        let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[2, 3, 4, 5]));
        fixture.assert_response(&response, None, Some((block_id, &[0, 1, 2, 3, 4, 5])));
        assert_eq!(recovery.metrics.recovered_stake, 200);
    }
}

#[test]
fn test_unverified_certificate_is_never_published() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    // Admitted certificates are trusted, so this one breaks the invariant that they verified:
    // its signature does not match its signers.
    let mut forged = fixture.certificate(vote, &[0, 1, 2, 3]);
    forged.signature = fixture.aggregate(vote, &[0]);
    recovery.add_certificate(forged, &fixture.rank_map);
    // It would cover the direct baseline with more stake, but fails the final check.
    wait_until_prepared(&mut recovery);
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[0, 1]));
    fixture.assert_response(&response, Some(&[0, 1]), None);
    assert_eq!(recovery.metrics.unverified_outputs, 1);
}

#[test]
fn test_recovery_never_drops_a_validator_the_baseline_pays() {
    let fixture = Fixture::new();
    let block_id = BlockId::new_unique();
    let vote = Vote::new_notarization_vote(Block {
        slot: SLOT,
        block_id,
    });
    let baseline = || fixture.response(vote, &[0, 1, 2, 3]);
    // The certificate has more stake than the direct baseline, but lacks rank 3.
    let certificate = || fixture.certificate(vote, &[0, 1, 2, 4, 5]);
    let mut recovery = Recovery::new();
    recovery.add_certificate(certificate(), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    let response = recovery.finish(SLOT, SHRED_VERSION, baseline());
    fixture.assert_response(&response, None, Some((block_id, &[0, 1, 2, 3])));
    assert_eq!(recovery.metrics.recovered_stake, 0);

    // An extension with rank 3's component keeps it, so it replaces the baseline.
    let mut recovery = Recovery::new();
    recovery.add_certificate(certificate(), &fixture.rank_map);
    recovery.add_signatures(vote, fixture.signatures(vote, &[3]), &fixture.rank_map);
    wait_for_check(&mut recovery);
    wait_until_prepared(&mut recovery);
    let response = recovery.finish(SLOT, SHRED_VERSION, baseline());
    fixture.assert_response(&response, None, Some((block_id, &[0, 1, 2, 3, 4, 5])));
    assert_eq!(recovery.metrics.recovered_stake, 200);
}

#[test]
fn test_split_optimistic_components_are_reverified_and_fall_back_on_failure() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut signatures = fixture.signatures(vote, &[0, 1, 2, 3, 4]);
    let delta = BlsKeypair::new().sign(b"perturbation");
    let delta: Signature = delta.into();
    signatures[0].1 = SignatureProjective::aggregate_with_weights(
        [&signatures[0].1, &delta].into_iter(),
        [Scalar::ONE, Scalar::ONE].iter(),
    )
    .unwrap()
    .into();
    signatures[4].1 = SignatureProjective::aggregate_with_weights(
        [&signatures[4].1, &delta].into_iter(),
        [Scalar::ONE, -Scalar::ONE].iter(),
    )
    .unwrap()
    .into();
    // The entire direct batch verifies because its invalid components cancel.
    let direct_signature: SignatureCompressed =
        SignatureProjective::aggregate(signatures.iter().map(|(_, signature)| signature))
            .unwrap()
            .into();
    let direct_bitmap = encode_base2(&fixture.ranks(&[0, 1, 2, 3, 4])).unwrap();
    verify_base2(
        &get_vote_payload_to_sign(vote, SHRED_VERSION),
        &direct_signature,
        &direct_bitmap,
        fixture.rank_map.len(),
        |rank| {
            fixture
                .rank_map
                .get_pubkey_stake_entry(rank)
                .map(|entry| entry.bls_pubkey)
        },
        None,
    )
    .unwrap();
    let baseline = RewardRespSucc {
        skip: Some(SkipRewardCertificate::try_new(SLOT, direct_signature, direct_bitmap).unwrap()),
        notar: None,
        validators: (0..5)
            .map(|rank| {
                fixture
                    .rank_map
                    .get_pubkey_stake_entry(rank)
                    .unwrap()
                    .vote_account_pubkey
            })
            .collect(),
    };
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    recovery.add_signatures(vote, signatures, &fixture.rank_map);
    wait_for_check(&mut recovery);
    assert_eq!(recovery.metrics.failed_checks, 1);
    assert!(recovery.slots[&SLOT].payloads[&vote].ready.is_empty());
    // The only outside component is the invalid one, so the failed check already isolates it.
    assert_eq!(recovery.metrics.checks, 2);
    assert_eq!(recovery.metrics.invalid_components, 1);
    assert!(
        !recovery.slots[&SLOT].payloads[&vote]
            .signatures
            .contains_key(&4)
    );
    // Replacing the inside component with a valid seed leaves an invalid outside signature.
    let response = recovery.finish(SLOT, SHRED_VERSION, baseline);
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4]), None);
    assert_eq!(recovery.metrics.recovered_stake, 0);
}

#[test]
fn test_mixed_fallback_certificates_are_excluded() {
    let fixture = Fixture::new();
    let block = Block::new_unique(SLOT);
    for (cert_type, primary, fallback) in [
        (
            CertificateType::Skip(SLOT),
            Vote::new_skip_vote(SLOT),
            Vote::new_skip_fallback_vote(SLOT),
        ),
        (
            CertificateType::NotarizeFallback(block),
            Vote::new_notarization_vote(block),
            Vote::new_notarization_fallback_vote(block),
        ),
    ] {
        let signatures = [
            fixture.aggregate(primary, &[0, 1, 2]),
            fixture.aggregate(fallback, &[3, 4, 5]),
        ];
        let certificate = Certificate {
            cert_type,
            signature: SignatureProjective::aggregate(signatures.iter())
                .unwrap()
                .into(),
            bitmap: encode_base3(&fixture.ranks(&[0, 1, 2]), &fixture.ranks(&[3, 4, 5])).unwrap(),
        };
        assert!(Candidate::from_certificate(certificate.clone(), &fixture.rank_map).is_none());
        let mut recovery = Recovery::new();
        recovery.add_certificate(certificate, &fixture.rank_map);
        assert!(recovery.slots.is_empty());
    }
}

#[test]
fn test_fast_finalize_and_primary_only_fallback_preserve_notar_payload() {
    let fixture = Fixture::new();
    let block = Block::new_unique(SLOT);
    let vote = Vote::new_notarization_vote(block);
    let mut fast = fixture.certificate(vote, &[0, 1, 2, 3, 4]);
    fast.cert_type = CertificateType::FinalizeFast(block);
    let mut primary_only = fixture.certificate(vote, &[0, 1, 2, 3]);
    primary_only.cert_type = CertificateType::NotarizeFallback(block);
    primary_only.bitmap = encode_base3(&fixture.ranks(&[0, 1, 2, 3]), &fixture.ranks(&[])).unwrap();
    for (certificate, ranks) in [
        (fast, vec![0, 1, 2, 3, 4]),
        (primary_only, vec![0, 1, 2, 3]),
    ] {
        let mut recovery = Recovery::new();
        recovery.add_certificate(certificate, &fixture.rank_map);
        wait_until_prepared(&mut recovery);
        let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
        fixture.assert_response(&response, None, Some((block.block_id, &ranks)));
    }
}

#[test]
fn test_selection_maximizes_unique_skip_and_notar_stake() {
    let fixture = Fixture::new();
    let skip = Vote::new_skip_vote(SLOT);
    let larger_block = Block::new_unique(SLOT);
    let complementary_block = Block::new_unique(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(skip, &[0, 1, 2, 3]), &fixture.rank_map);
    recovery.add_certificate(
        fixture.certificate(Vote::new_notarization_vote(larger_block), &[0, 1, 2, 3, 4]),
        &fixture.rank_map,
    );
    recovery.add_certificate(
        fixture.certificate(
            Vote::new_notarization_vote(complementary_block),
            &[2, 3, 4, 5],
        ),
        &fixture.rank_map,
    );
    wait_until_prepared(&mut recovery);
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(
        &response,
        Some(&[0, 1, 2, 3]),
        Some((complementary_block.block_id, &[2, 3, 4, 5])),
    );
    assert_eq!(recovery.metrics.recovered_stake, 600);
}

#[test]
fn test_weighted_byte_selection_matches_naive_selection() {
    fn naive_selection(
        candidates: &[Candidate],
        rank_map: &BLSPubkeyToRankMap,
        required: &BitVec<u8>,
    ) -> Option<(Option<usize>, Option<usize>, BitVec<u8>)> {
        let skips: Vec<_> = std::iter::once(None)
            .chain(
                candidates
                    .iter()
                    .enumerate()
                    .filter_map(|(index, candidate)| {
                        matches!(candidate.vote, Vote::Skip(_)).then_some(Some(index))
                    }),
            )
            .collect();
        let notars: Vec<_> = std::iter::once(None)
            .chain(
                candidates
                    .iter()
                    .enumerate()
                    .filter_map(|(index, candidate)| {
                        matches!(candidate.vote, Vote::Notarize(_)).then_some(Some(index))
                    }),
            )
            .collect();
        let mut best = None;
        let mut best_stake = 0;
        for skip in skips {
            for &notar in &notars {
                let mut union = BitVec::<u8>::repeat(false, rank_map.len());
                for index in [skip, notar].into_iter().flatten() {
                    for rank in candidates[index].ranks.iter_ones() {
                        union.set(rank, true);
                    }
                }
                if required.iter_ones().any(|rank| !union[rank]) {
                    continue;
                }
                let stake = union.iter_ones().fold(0u64, |sum, rank| {
                    sum.saturating_add(
                        rank_map
                            .get_pubkey_stake_entry(rank)
                            .unwrap()
                            .stake
                            .get()
                            .get(),
                    )
                });
                if stake > best_stake {
                    best_stake = stake;
                    best = Some((skip, notar, union));
                }
            }
        }
        best
    }

    for count in [1, 3, 7, 8, 9, 15, 31] {
        let fixture = Fixture::new_with_stakes(
            SLOT,
            EpochSchedule::without_warmup(),
            (1..=count).map(|rank| (rank * rank + 3) as u64).collect(),
        );
        let votes = [
            Vote::new_skip_vote(SLOT),
            Vote::new_notarization_vote(Block::new_unique(SLOT)),
            Vote::new_notarization_vote(Block::new_unique(SLOT)),
            Vote::new_notarization_vote(Block::new_unique(SLOT)),
        ];
        // This tests selection, not cryptography: only the signer sets and their exact source
        // stakes affect its result. End-to-end tests verify actual published signatures.
        let signatures: Vec<SignatureCompressed> = votes
            .iter()
            .map(|&vote| fixture.aggregate(vote, &[0]).try_into().unwrap())
            .collect();
        let mut rng = StdRng::seed_from_u64(count as u64);
        for case in 0usize..96 {
            let mut candidates = vec![];
            for (kind, (&vote, &signature)) in votes.iter().zip(&signatures).enumerate() {
                for _ in 0..rng.random_range(0..=if kind == 0 { 8 } else { 4 }) {
                    let mut ranks = BitVec::<u8>::repeat(false, count + 3);
                    for rank in 0..count {
                        // Some cases permanently omit rank 0, so the optimum never reaches all
                        // stake. Others exercise full coverage and large overlaps.
                        let included =
                            (!case.is_multiple_of(3) || rank != 0) && rng.random_bool(0.7);
                        ranks.set(rank + 3, included);
                    }
                    let ranks = ranks.split_off(3);
                    if let Some(mut candidate) =
                        Candidate::new(vote, signature, ranks, &fixture.rank_map)
                    {
                        let Decoded::Base2(mut decoded) = decode(&candidate.bitmap, count).unwrap()
                        else {
                            panic!("expected base2 candidate bitmap");
                        };
                        decoded.resize(count, false);
                        assert_eq!(decoded, candidate.ranks);
                        if !count.is_multiple_of(8) {
                            // Dead storage bits must not contribute stake or required coverage.
                            *candidate.ranks.as_raw_mut_slice().last_mut().unwrap() |=
                                !((1u8 << (count % 8)) - 1);
                        }
                        candidates.push(candidate);
                    }
                }
            }
            // Explicit duplicate evidence exercises the original first-wins tie ordering.
            if let Some(candidate) = candidates.first().cloned() {
                candidates.push(candidate);
            }
            let mut required = fixture.ranks(
                &(0..count)
                    .filter(|_| !case.is_multiple_of(4) && rng.random_bool(0.3))
                    .collect::<Vec<_>>(),
            );
            if case.is_multiple_of(5) {
                required.truncate(count / 2);
            }
            if !required.len().is_multiple_of(8) {
                let tail = required.len() % 8;
                *required.as_raw_mut_slice().last_mut().unwrap() |= !((1u8 << tail) - 1);
            }
            let expected = naive_selection(&candidates, &fixture.rank_map, &required);
            let actual = select_pair(&candidates, &fixture.rank_map, &required).map(
                |(skip, notar, ranks)| {
                    let index = |candidate: &Candidate| {
                        candidates
                            .iter()
                            .position(|entry| std::ptr::eq(entry, candidate))
                            .unwrap()
                    };
                    (skip.map(index), notar.map(index), ranks)
                },
            );
            assert_eq!(actual, expected, "count={count}, case={case}");
        }
    }
}

#[test]
fn test_certificate_storage_is_bounded_and_duplicate_bitmaps_do_not_use_budget() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    let first = fixture.certificate(vote, &[0, 1, 2, 3]);
    recovery.add_certificate(first.clone(), &fixture.rank_map);
    recovery.add_certificate(first, &fixture.rank_map);
    assert_eq!(recovery.slots[&SLOT].num_certificates, 1);
    for ranks in [
        &[0, 1, 2, 4],
        &[0, 1, 2, 5],
        &[0, 1, 3, 4],
        &[0, 1, 3, 5],
        &[0, 1, 4, 5],
    ] {
        recovery.add_certificate(fixture.certificate(vote, ranks), &fixture.rank_map);
    }
    let slot = &recovery.slots[&SLOT];
    assert_eq!(slot.num_certificates, MAX_CERTIFICATES_PER_SLOT);
    assert_eq!(slot.payloads[&vote].seeds.len(), MAX_CERTIFICATES_PER_SLOT);
}

#[test]
fn test_raw_signature_storage_is_bounded_and_keeps_first_component() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let signatures = fixture.signatures(vote, &[0, 1, 2, 3, 4, 5]);
    let first_signature = signatures[0].1;
    let mut recovery = Recovery::new();
    recovery.add_signatures(vote, signatures, &fixture.rank_map);
    let generation = recovery.slots[&SLOT].payloads[&vote].generation;
    let replacement: Signature = fixture.keypairs[0].sign(b"replacement").into();
    recovery.add_signatures(
        vote,
        [
            (0, replacement),
            (u16::try_from(NUM_VALIDATORS).unwrap(), replacement),
        ],
        &fixture.rank_map,
    );
    let state = &recovery.slots[&SLOT].payloads[&vote];
    assert_eq!(state.signatures.len(), fixture.rank_map.len());
    assert_eq!(state.signatures[&0], first_signature);
    assert_eq!(state.generation, generation);
    assert_eq!(recovery.metrics.invalid_signature_ranks, 1);
    for _ in 0..MAX_PAYLOADS_PER_SLOT + 2 {
        let vote = Vote::new_notarization_vote(Block::new_unique(SLOT));
        recovery.add_signatures(vote, fixture.signatures(vote, &[0]), &fixture.rank_map);
    }
    assert_eq!(recovery.slots[&SLOT].payloads.len(), MAX_PAYLOADS_PER_SLOT);
    assert_eq!(recovery.metrics.dropped_payloads, 3);
}

#[test]
fn test_finished_or_pruned_slots_are_not_recreated_by_worker_results() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    for prune in [false, true] {
        let mut recovery = Recovery::new();
        recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
        recovery.add_signatures(vote, fixture.signatures(vote, &[4, 5]), &fixture.rank_map);
        recovery.poll(SHRED_VERSION);
        assert!(recovery.in_flight);
        if prune {
            recovery.retain_from(SLOT.saturating_add(1));
        } else {
            let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
            fixture.assert_response(&response, None, None);
        }
        wait_for_check(&mut recovery);
        assert_eq!(recovery.metrics.late_results, 1);
        assert!(!recovery.slots.contains_key(&SLOT));
        assert_eq!(
            recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default()),
            RewardRespSucc::default()
        );
    }
}

#[test]
fn test_builder_accepts_finalized_source_slot_using_its_epoch_rank_map() {
    let source_slot = 124;
    let fixture = Fixture::new_at(source_slot, EpochSchedule::custom(128, 128, false));
    let mut root_bank = Bank::new_from_parent(fixture.bank.clone(), SlotLeader::default(), 130);
    assert_eq!(root_bank.epoch(), 1);
    // Reverse the ordering in epoch 1 so using the current epoch's ranks gives wrong recipients
    // and fails signature verification. The source slot remains in epoch 0.
    let next_epoch_accounts = fixture
        .bank
        .epoch_stakes(0)
        .unwrap()
        .stakes()
        .vote_accounts()
        .as_ref()
        .iter()
        .map(|(key, (_, account))| {
            let rank = *fixture.rank_map.get_rank_for_vote_pubkey(key).unwrap();
            (*key, ((u64::from(rank) + 1) * 100, account.clone()))
        })
        .collect();
    root_bank.set_epoch_stakes_for_test(
        1,
        VersionedEpochStakes::new_for_tests(next_epoch_accounts, 1),
    );
    assert_ne!(
        fixture
            .rank_map
            .get_pubkey_stake_entry(0)
            .unwrap()
            .vote_account_pubkey,
        root_bank
            .get_rank_map(root_bank.slot())
            .unwrap()
            .get_pubkey_stake_entry(0)
            .unwrap()
            .vote_account_pubkey,
    );
    let vote = Vote::new_skip_vote(source_slot);
    let mut builder = builder();
    builder.handle_input(
        &root_bank,
        RewardInput::Certificates(vec![fixture.certificate(vote, &[0, 1, 2, 3])]),
    );
    wait_until_prepared(builder.recovery_mut());
    let response = builder
        .build_certs(source_slot.saturating_add(NUM_SLOTS_FOR_REWARD))
        .unwrap();
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
}

#[test]
fn test_builder_rejects_late_evidence_for_sealed_slot_even_while_root_lags() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut builder = builder();
    // Reporting would reset the counters this test reads, however slowly it runs.
    builder.metrics.last_report = Instant::now().checked_add(Duration::from_secs(60)).unwrap();
    builder.handle_input(
        &fixture.bank,
        RewardInput::Certificates(vec![fixture.certificate(vote, &[0, 1, 2, 3])]),
    );
    wait_until_prepared(builder.recovery_mut());
    let response = builder
        .build_certs(SLOT.saturating_add(NUM_SLOTS_FOR_REWARD))
        .unwrap();
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
    assert_eq!(fixture.bank.slot(), SLOT);
    let own_vote = VoteMessage {
        vote,
        signature: fixture.signatures(vote, &[5])[0].1.try_into().unwrap(),
        rank: 5,
        stake: fixture.rank_map.get_pubkey_stake_entry(5).unwrap().stake,
    };
    let external_vote = VoteMessage {
        vote,
        signature: fixture.signatures(vote, &[4])[0].1.try_into().unwrap(),
        rank: 4,
        stake: fixture.rank_map.get_pubkey_stake_entry(4).unwrap().stake,
    };
    for input in [
        RewardInput::Certificates(vec![fixture.certificate(vote, &[2, 3, 4, 5])]),
        RewardInput::VoteSignatures {
            vote,
            signatures: fixture.signatures(vote, &[4, 5]),
        },
        RewardInput::External(vec![VoteAggregate::new_from_verified_vote(
            NUM_VALIDATORS,
            external_vote,
        )]),
        RewardInput::Own(own_vote),
    ] {
        builder.handle_input(&fixture.bank, input);
        assert!(!builder.aggregates.contains_key(&SLOT));
        assert!(!builder.recovery_mut().slots.contains_key(&SLOT));
    }
    assert_eq!(builder.metrics.late_vote_aggregates, 2);
    assert_eq!(builder.metrics.recovery.late_certificates, 1);
    assert_eq!(builder.metrics.recovery.late_signature_components, 2);
    assert_eq!(
        builder
            .build_certs(SLOT.saturating_add(NUM_SLOTS_FOR_REWARD).saturating_add(1))
            .unwrap(),
        RewardRespSucc::default(),
    );
}

#[test]
fn test_builder_uses_verified_ready_candidate_while_newer_worker_job_is_blocked() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let certificate = fixture.certificate(vote, &[0, 1, 2, 3]);
    let mut ready = Job {
        vote,
        generation: 1,
        rank_map: fixture.rank_map.clone(),
        shred_version: SHRED_VERSION,
        seeds: vec![Candidate::from_certificate(certificate.clone(), &fixture.rank_map).unwrap()],
        checked_seeds: vec![],
        verification_gate: None,
        signatures: fixture.signatures(vote, &[4]).into_iter().collect(),
        ready: vec![],
    }
    .run()
    .candidates;
    let ready = ready.pop().unwrap();
    let mut builder = builder();
    let (jobs_sender, jobs_receiver) = bounded::<Job>(1);
    let (results_sender, results_receiver) = bounded(1);
    let (started_sender, started_receiver) = bounded(1);
    // This sender drops before the builder on panic, so the worker cannot strand Drop::join.
    let (release_sender, release_receiver) = bounded(1);
    let worker = thread::spawn(move || {
        if let Ok(job) = jobs_receiver.recv() {
            started_sender.send(()).unwrap();
            let _ = release_receiver.recv();
            let _ = results_sender.send(job.run());
        }
    });
    builder.recovery = Some(Recovery::from_worker(jobs_sender, results_receiver, worker));
    builder.handle_input(&fixture.bank, RewardInput::Certificates(vec![certificate]));
    builder
        .recovery_mut()
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&vote)
        .unwrap()
        .ready = vec![ready];
    builder.handle_input(
        &fixture.bank,
        RewardInput::VoteSignatures {
            vote,
            signatures: fixture.signatures(vote, &[4, 5]),
        },
    );
    started_receiver
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    let response = builder
        .build_certs(SLOT.saturating_add(NUM_SLOTS_FOR_REWARD))
        .unwrap();
    assert!(builder.recovery_mut().in_flight);
    release_sender.send(()).unwrap();
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4]), None);
    wait_for_check(builder.recovery_mut());
    assert!(!builder.recovery_mut().slots.contains_key(&SLOT));
}

#[test]
fn test_rate_limited_updates_refresh_after_more_than_sixteen_completed_jobs() {
    let fixture = Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), 44);
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(
        fixture.certificate(vote, &(0..27).collect::<Vec<_>>()),
        &fixture.rank_map,
    );
    for (index, rank) in (27..43).enumerate() {
        recovery.add_signatures(vote, fixture.signatures(vote, &[rank]), &fixture.rank_map);
        recovery
            .slots
            .get_mut(&SLOT)
            .unwrap()
            .payloads
            .get_mut(&vote)
            .unwrap()
            .last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
        wait_for_completed_jobs(&mut recovery, u64::try_from(index + 1).unwrap());
    }
    assert_eq!(recovery.metrics.completed_jobs, 16);
    recovery.add_signatures(vote, fixture.signatures(vote, &[43]), &fixture.rank_map);
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&vote)
        .unwrap()
        .last_check = Some(Instant::now().checked_add(Duration::from_secs(60)).unwrap());
    recovery.poll(SHRED_VERSION);
    assert!(!recovery.in_flight);
    assert_eq!(recovery.metrics.rate_limited_updates, 1);
    recovery.poll(SHRED_VERSION);
    assert_eq!(recovery.metrics.rate_limited_updates, 1);
    // Advance only this payload's scheduling clock; no sleeps or wall-clock budget exhaustion.
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&vote)
        .unwrap()
        .last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
    wait_for_completed_jobs(&mut recovery, 17);
    assert_eq!(recovery.metrics.checks, 18);
    assert_eq!(recovery.metrics.worker_us.count(), 17);
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&(0..44).collect::<Vec<_>>()), None);
}

#[test]
fn test_scheduler_serves_new_payload_before_rechecking_hot_older_slot() {
    let fixture = Fixture::new();
    let hot = Vote::new_skip_vote(SLOT);
    let new = Vote::new_skip_vote(SLOT + 1);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(hot, &[0, 1, 2, 3]), &fixture.rank_map);
    recovery.add_signatures(hot, fixture.signatures(hot, &[4]), &fixture.rank_map);
    wait_for_check(&mut recovery);
    recovery.add_signatures(hot, fixture.signatures(hot, &[5]), &fixture.rank_map);
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&hot)
        .unwrap()
        .last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
    recovery.add_certificate(fixture.certificate(new, &[0, 1, 2, 3]), &fixture.rank_map);
    recovery.add_signatures(new, fixture.signatures(new, &[4]), &fixture.rank_map);
    recovery.poll(SHRED_VERSION);
    let hot_state = &recovery.slots[&SLOT].payloads[&hot];
    assert_ne!(hot_state.scheduled_generation, hot_state.generation);
    let new_state = &recovery.slots[&(SLOT + 1)].payloads[&new];
    assert_eq!(new_state.scheduled_generation, new_state.generation);
    wait_for_completed_jobs(&mut recovery, 2);
    assert!(!recovery.slots[&(SLOT + 1)].payloads[&new].ready.is_empty());
}

#[test]
fn test_failed_largest_seed_falls_back_to_seed_covering_invalid_components() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut signatures = fixture.signatures(vote, &[0, 4, 5]);
    let delta: Signature = BlsKeypair::new().sign(b"cancelling components").into();
    for ((_, signature), weight) in signatures.iter_mut().zip([Scalar::ONE, -Scalar::ONE]) {
        *signature = SignatureProjective::aggregate_with_weights(
            [&*signature, &delta].into_iter(),
            [Scalar::ONE, weight].iter(),
        )
        .unwrap()
        .into();
    }
    let mut recovery = Recovery::new();
    // This larger prospective union splits the cancelling pair and fails verification. Bisection
    // isolates rank 4's invalid component and keeps the valid rank 5 extension.
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    // Covering both components makes the smaller extension safe, despite the failed first seed.
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 4]), &fixture.rank_map);
    recovery.add_signatures(vote, signatures, &fixture.rank_map);
    wait_for_check(&mut recovery);
    assert_eq!(recovery.metrics.checks, 6);
    assert_eq!(recovery.metrics.failed_checks, 2);
    assert_eq!(recovery.metrics.seed_fallbacks, 2);
    assert_eq!(recovery.metrics.invalid_components, 1);
    let state = &recovery.slots[&SLOT].payloads[&vote];
    assert!(!state.signatures.contains_key(&4));
    assert_eq!(state.ready.len(), 2);
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[0, 4, 5]));
    // The direct baseline pays rank 4 because its batch verified as a whole, but its own
    // component was proven invalid, so the floor no longer requires it. Both extensions add rank
    // 5, and the bisected extension of the larger seed is prepared first.
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 5]), None);
}

#[test]
fn test_bisection_recovers_honest_components_behind_a_cancelling_pair() {
    let fixture = Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), 10);
    let vote = Vote::new_skip_vote(SLOT);
    // Colluding ranks 2 and 7 submit invalid components that cancel in their optimistic batch.
    let mut signatures = fixture.signatures(vote, &[2, 5, 6, 7, 8, 9]);
    let delta: Signature = BlsKeypair::new().sign(b"cancelling components").into();
    for (index, weight) in [(0, Scalar::ONE), (3, -Scalar::ONE)] {
        signatures[index].1 = SignatureProjective::aggregate_with_weights(
            [&signatures[index].1, &delta].into_iter(),
            [Scalar::ONE, weight].iter(),
        )
        .unwrap()
        .into();
    }
    let mut recovery = Recovery::new();
    // The seed covers rank 2, so only rank 7's invalid component reaches the extension.
    recovery.add_certificate(
        fixture.certificate(vote, &[0, 1, 2, 3, 4]),
        &fixture.rank_map,
    );
    recovery.add_signatures(vote, signatures, &fixture.rank_map);
    wait_for_check(&mut recovery);
    // Complete extension, ranks [5, 6], [7] and [8, 9], then the combined remainder.
    assert_eq!(recovery.metrics.checks, 6);
    assert_eq!(recovery.metrics.failed_checks, 2);
    assert_eq!(recovery.metrics.invalid_components, 1);
    assert!(
        !recovery.slots[&SLOT].payloads[&vote]
            .signatures
            .contains_key(&7)
    );
    // Eviction schedules another check, which finds the remainder already prepared.
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&vote)
        .unwrap()
        .last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
    wait_for_completed_jobs(&mut recovery, 2);
    assert_eq!(recovery.metrics.checks, 6);
    // The optimistic batch verified as a whole, so the direct baseline pays both colluders.
    // Rank 7's own signature is proven invalid, so it cannot keep the honest seed signers out.
    let response = recovery.finish(
        SLOT,
        SHRED_VERSION,
        fixture.response(vote, &[2, 5, 6, 7, 8, 9]),
    );
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4, 5, 6, 8, 9]), None);
}

#[test]
fn test_undecodable_component_is_evicted_without_a_check() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    let mut signatures = fixture.signatures(vote, &[4]);
    signatures.push((5, Signature([0xff; BLS_SIGNATURE_AFFINE_SIZE])));
    recovery.add_signatures(vote, signatures, &fixture.rank_map);
    wait_for_check(&mut recovery);
    assert_eq!(recovery.metrics.checks, 2);
    assert_eq!(recovery.metrics.failed_checks, 0);
    assert_eq!(recovery.metrics.invalid_components, 1);
    assert!(
        !recovery.slots[&SLOT].payloads[&vote]
            .signatures
            .contains_key(&5)
    );
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4]), None);
}

#[test]
fn test_bisection_budget_bounds_each_job_and_still_evicts_invalid_components() {
    let fixture = Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), 24);
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0]), &fixture.rank_map);
    // Every component signs another payload, so isolating all of them exceeds one job's budget.
    let invalid_ranks = (1..24).collect::<Vec<_>>();
    recovery.add_signatures(
        vote,
        fixture.signatures(Vote::new_skip_vote(SLOT + 1), &invalid_ranks),
        &fixture.rank_map,
    );
    wait_for_check(&mut recovery);
    // Bisection leaves one check for combining verified parts, and no part verifies here.
    assert_eq!(recovery.metrics.checks, MAX_CHECKS_PER_JOB - 1);
    let evicted = recovery.metrics.invalid_components;
    assert!(evicted > 0);
    assert!(evicted < invalid_ranks.len() as u64);
    // Eviction schedules another job, which isolates the rest within its own budget.
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&vote)
        .unwrap()
        .last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
    wait_for_completed_jobs(&mut recovery, 2);
    assert_eq!(
        recovery.metrics.invalid_components,
        invalid_ranks.len() as u64
    );
    assert_eq!(recovery.metrics.failed_checks, recovery.metrics.checks - 1);
    assert!(recovery.slots[&SLOT].payloads[&vote].signatures.is_empty());
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0]), None);
}

#[test]
fn test_randomized_recovery_keeps_honest_components_and_publishes_only_verified_coverage() {
    const VALIDATORS: usize = 12;
    let fixture =
        Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), VALIDATORS);
    let vote = Vote::new_skip_vote(SLOT);
    let honest = fixture.signatures(vote, &(0..VALIDATORS).collect::<Vec<_>>());
    let certificate = |ranks: &[usize]| Certificate {
        cert_type: CertificateType::Skip(SLOT),
        signature: SignatureProjective::aggregate(ranks.iter().map(|&rank| &honest[rank].1))
            .unwrap()
            .into(),
        bitmap: encode_base2(&fixture.ranks(ranks)).unwrap(),
    };
    let perturb = |signature: &Signature, delta: &Signature, weight: Scalar| -> Signature {
        SignatureProjective::aggregate_with_weights(
            [signature, delta].into_iter(),
            [Scalar::ONE, weight].iter(),
        )
        .unwrap()
        .into()
    };
    for case in 0..24u64 {
        let mut rng = StdRng::seed_from_u64(case);
        let seeds: Vec<Vec<usize>> = (0..rng.random_range(1..=3))
            .map(|_| {
                let ranks: Vec<_> = (0..VALIDATORS).filter(|_| rng.random_bool(0.5)).collect();
                if ranks.is_empty() { vec![0] } else { ranks }
            })
            .collect();
        let mut included: Vec<_> = (0..VALIDATORS).filter(|_| rng.random_bool(0.75)).collect();
        included.shuffle(&mut rng);
        let delta: Signature = BlsKeypair::new().sign(&case.to_le_bytes()).into();
        let mut components: Vec<_> = included.iter().map(|&rank| honest[rank]).collect();
        let mut invalid = HashSet::new();
        // Cancelling pairs, then lone invalid components, then possibly an undecodable one.
        let pairs = rng.random_range(0..=2).min(components.len() / 2);
        for pair in 0..pairs {
            for (index, weight) in [(2 * pair, Scalar::ONE), (2 * pair + 1, -Scalar::ONE)] {
                components[index].1 = perturb(&components[index].1, &delta, weight);
                invalid.insert(components[index].0);
            }
        }
        let lone = rng
            .random_range(0..=2)
            .min(components.len().saturating_sub(2 * pairs));
        for (rank, signature) in components.iter_mut().skip(2 * pairs).take(lone) {
            *signature = perturb(signature, &delta, Scalar::ONE);
            invalid.insert(*rank);
        }
        if 2 * pairs + lone < components.len() && rng.random_bool(0.3) {
            let index = 2 * pairs + lone;
            components[index].1 = Signature([0xff; BLS_SIGNATURE_AFFINE_SIZE]);
            invalid.insert(components[index].0);
        }
        let honest_ranks: HashSet<_> = included
            .iter()
            .map(|&rank| u16::try_from(rank).unwrap())
            .filter(|rank| !invalid.contains(rank))
            .collect();

        let mut recovery = Recovery::new();
        for ranks in &seeds {
            recovery.add_certificate(certificate(ranks), &fixture.rank_map);
        }
        recovery.add_signatures(vote, components, &fixture.rank_map);
        // Run jobs until nothing changed is left to check, bypassing only the pacing interval. A
        // scheduled job no longer needs a check, so wait for any job in flight as well.
        for _ in 0..32 {
            if !recovery.in_flight {
                let state = recovery
                    .slots
                    .get_mut(&SLOT)
                    .unwrap()
                    .payloads
                    .get_mut(&vote)
                    .unwrap();
                if !state.needs_check() {
                    break;
                }
                state.last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
            }
            let completed = recovery.metrics.completed_jobs;
            wait_for_completed_jobs(&mut recovery, completed + 1);
        }
        assert!(!recovery.in_flight, "case {case} did not converge");
        let state = &recovery.slots[&SLOT].payloads[&vote];
        assert!(!state.needs_check(), "case {case} did not converge");
        for rank in &honest_ranks {
            assert!(
                state.signatures.contains_key(rank),
                "case {case} evicted honest rank {rank}",
            );
        }

        let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
        let skip = response.skip.as_ref().unwrap();
        let Decoded::Base2(published) = decode(skip.to_bitmap(), VALIDATORS).unwrap() else {
            panic!("reward output must use base2");
        };
        let published: Vec<_> = published.iter_ones().collect();
        // Verifies the aggregate and the exact validator list, as a replayer would.
        fixture.assert_response(&response, Some(&published), None);
        // Equal stakes: each seed extended by its honest outside components is a lower bound.
        for ranks in &seeds {
            let mut covered: HashSet<_> = ranks.iter().copied().collect();
            covered.extend(honest_ranks.iter().map(|&rank| usize::from(rank)));
            assert!(
                published.len() >= covered.len(),
                "case {case}: published {published:?}, seed {ranks:?} with honest {honest_ranks:?}",
            );
        }
    }
}

#[test]
fn test_exited_worker_stops_scheduling_and_keeps_prepared_candidates() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let (jobs_sender, jobs_receiver) = bounded::<Job>(1);
    let (results_sender, results_receiver) = bounded::<JobResult>(1);
    // The worker completes one job, then exits without a result. To the service that looks the
    // same as a worker that panicked.
    let worker = thread::spawn(move || {
        if let Ok(job) = jobs_receiver.recv() {
            let _ = results_sender.send(job.run());
        }
        let _job = jobs_receiver.recv();
        drop(results_sender);
    });
    let mut recovery = Recovery::from_worker(jobs_sender, results_receiver, worker);
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    recovery.add_signatures(vote, fixture.signatures(vote, &[4]), &fixture.rank_map);
    wait_for_check(&mut recovery);
    assert_eq!(recovery.slots[&SLOT].payloads[&vote].ready.len(), 1);
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&vote)
        .unwrap()
        .last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
    recovery.add_signatures(vote, fixture.signatures(vote, &[5]), &fixture.rank_map);
    recovery.poll(SHRED_VERSION);
    assert!(recovery.in_flight);
    let start = Instant::now();
    while recovery.metrics.worker_exits == 0 {
        recovery.poll(SHRED_VERSION);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "worker exit was not observed"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!recovery.in_flight);
    assert_eq!(recovery.poll_interval(), Duration::from_secs(1));
    // Later evidence neither schedules work nor makes the service poll at the busy interval.
    recovery.add_signatures(vote, fixture.signatures(vote, &[5]), &fixture.rank_map);
    recovery.poll(SHRED_VERSION);
    assert!(!recovery.in_flight);
    assert_eq!(recovery.poll_interval(), Duration::from_secs(1));
    assert_eq!(recovery.metrics.worker_exits, 1);
    // The candidate prepared before the exit is still published.
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4]), None);
}

#[test]
fn test_exhausted_bisection_budget_still_combines_verified_parts() {
    let fixture = Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), 24);
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0]), &fixture.rank_map);
    // Isolating all five invalid components takes more than one job's budget.
    let invalid_ranks = [2, 4, 7, 13, 18];
    let honest_ranks: Vec<_> = (1..24)
        .filter(|rank| !invalid_ranks.contains(rank))
        .collect();
    let mut signatures = fixture.signatures(vote, &honest_ranks);
    signatures.extend(fixture.signatures(Vote::new_skip_vote(SLOT + 1), &invalid_ranks));
    recovery.add_signatures(vote, signatures, &fixture.rank_map);
    wait_for_check(&mut recovery);
    // The check left for combining the verified parts is used, so this job publishes them.
    assert_eq!(recovery.metrics.checks, MAX_CHECKS_PER_JOB);
    assert!(recovery.metrics.invalid_components > 0);
    assert!(recovery.metrics.invalid_components < invalid_ranks.len() as u64);
    let ready = &recovery.slots[&SLOT].payloads[&vote].ready;
    assert_eq!(ready.len(), 1);
    assert!(ready[0].ranks.count_ones() > 2);
    assert!(invalid_ranks.iter().all(|&rank| !ready[0].ranks[rank]));
}

#[test]
fn test_raw_payloads_for_made_up_blocks_cannot_keep_out_stronger_payloads() {
    let fixture = Fixture::new();
    let mut recovery = Recovery::new();
    // Each colluder's own vote for a made-up block opens a payload with one raw component.
    for rank in 0..MAX_PAYLOADS_PER_SLOT {
        let made_up = Vote::new_notarization_vote(Block::new_unique(SLOT));
        recovery.add_signatures(
            made_up,
            fixture.signatures(made_up, &[rank]),
            &fixture.rank_map,
        );
    }
    let skip = Vote::new_skip_vote(SLOT);
    recovery.add_signatures(skip, fixture.signatures(skip, &[4, 5]), &fixture.rank_map);
    assert!(recovery.slots[&SLOT].payloads.contains_key(&skip));
    assert_eq!(recovery.slots[&SLOT].payloads.len(), MAX_PAYLOADS_PER_SLOT);
    assert_eq!(recovery.metrics.dropped_payloads, 1);
    // A payload no stronger than the weakest one does not displace it.
    let weak = Vote::new_notarization_vote(Block::new_unique(SLOT));
    recovery.add_signatures(weak, fixture.signatures(weak, &[5]), &fixture.rank_map);
    assert!(!recovery.slots[&SLOT].payloads.contains_key(&weak));
    assert_eq!(recovery.metrics.dropped_payloads, 2);
    // A certificate for a new payload displaces a made-up payload, not the stronger skip payload.
    let notar = Vote::new_notarization_vote(Block::new_unique(SLOT));
    recovery.add_certificate(fixture.certificate(notar, &[0, 1, 2, 3]), &fixture.rank_map);
    let payloads = &recovery.slots[&SLOT].payloads;
    assert!(payloads.contains_key(&notar));
    assert!(payloads.contains_key(&skip));
    assert_eq!(payloads.len(), MAX_PAYLOADS_PER_SLOT);
}

#[test]
fn test_own_vote_extends_an_external_certificate() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut builder = builder();
    builder.handle_input(
        &fixture.bank,
        RewardInput::Certificates(vec![fixture.certificate(vote, &[0, 1, 2, 3])]),
    );
    let own_vote = VoteMessage {
        vote,
        signature: fixture.signatures(vote, &[4])[0].1.try_into().unwrap(),
        rank: 4,
        stake: fixture.rank_map.get_pubkey_stake_entry(4).unwrap().stake,
    };
    builder.handle_input(&fixture.bank, RewardInput::Own(own_vote));
    wait_until_prepared(builder.recovery_mut());
    let response = builder
        .build_certs(SLOT.saturating_add(NUM_SLOTS_FOR_REWARD))
        .unwrap();
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4]), None);
}

#[test]
fn test_unchanged_prepared_extension_is_not_reverified() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2]), &fixture.rank_map);
    recovery.add_certificate(fixture.certificate(vote, &[3, 4, 5]), &fixture.rank_map);
    recovery.add_signatures(vote, fixture.signatures(vote, &[0, 3]), &fixture.rank_map);
    wait_for_check(&mut recovery);
    assert_eq!(recovery.metrics.checks, 4);
    // Rank 1 extends only the second seed. The first seed's prospective set is unchanged and
    // already prepared, and the larger new extension does not cover it.
    recovery.add_signatures(vote, fixture.signatures(vote, &[1]), &fixture.rank_map);
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&vote)
        .unwrap()
        .last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
    wait_for_completed_jobs(&mut recovery, 2);
    assert_eq!(recovery.metrics.checks, 5);
    assert_eq!(recovery.metrics.failed_checks, 0);
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0, 1, 3, 4, 5]), None);
}

#[test]
fn test_extension_covered_by_another_seed_is_not_checked() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2]), &fixture.rank_map);
    recovery.add_certificate(
        fixture.certificate(vote, &[0, 1, 2, 3, 4, 5]),
        &fixture.rank_map,
    );
    // Only the smaller seed misses these components, and the larger seed covers its extension.
    recovery.add_signatures(vote, fixture.signatures(vote, &[3, 4]), &fixture.rank_map);
    wait_for_check(&mut recovery);
    assert_eq!(recovery.metrics.checks, 2);
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4, 5]), None);
}

#[test]
fn test_request_cutoff_excludes_certificates_checked_after_the_baseline() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    let cutoff = Instant::now();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    let response = recovery.finish_at(SLOT, SHRED_VERSION, RewardRespSucc::default(), cutoff);
    fixture.assert_response(&response, Some(&[0, 1]), None);
}

#[test]
fn test_request_cutoff_preserves_baseline_floor_until_component_failure_was_proved() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 4]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    let cutoff = Instant::now();
    recovery.add_signatures(
        vote,
        fixture.signatures(Vote::new_skip_vote(SLOT + 1), &[3]),
        &fixture.rank_map,
    );
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.invalid_components, 1);
    // The older seed would win on stake if a failure proved after the request could relax its
    // required signer floor. The direct baseline must still pay rank 3 at the earlier cutoff.
    let response = recovery.finish_at(SLOT, SHRED_VERSION, fixture.response(vote, &[2, 3]), cutoff);
    fixture.assert_response(&response, Some(&[2, 3]), None);
}

#[test]
fn test_components_inside_every_seed_schedule_no_job() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2]), &fixture.rank_map);
    recovery.add_signatures(vote, fixture.signatures(vote, &[0, 1]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    recovery.poll(SHRED_VERSION);
    assert!(!recovery.in_flight);
    // Later polls skip the payload until it changes.
    let state = &recovery.slots[&SLOT].payloads[&vote];
    assert_eq!(state.scheduled_generation, state.generation);
}

#[test]
fn test_smaller_prepared_notar_candidate_can_maximize_skip_union() {
    let fixture = Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), 10);
    let block = Block::new_unique(SLOT);
    let notar = Vote::new_notarization_vote(block);
    let skip = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(
        fixture.certificate(notar, &[0, 1, 2, 3, 4, 5, 6]),
        &fixture.rank_map,
    );
    recovery.add_certificate(
        fixture.certificate(notar, &[3, 4, 5, 6, 7, 8]),
        &fixture.rank_map,
    );
    recovery.add_signatures(notar, fixture.signatures(notar, &[9]), &fixture.rank_map);
    wait_for_check(&mut recovery);
    assert_eq!(recovery.metrics.checks, 4);
    assert_eq!(recovery.slots[&SLOT].payloads[&notar].ready.len(), 2);
    // The smaller notar extension adds more validators outside the direct skip set.
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(skip, &[0, 1, 2]));
    fixture.assert_response(
        &response,
        Some(&[0, 1, 2]),
        Some((block.block_id, &[3, 4, 5, 6, 7, 8, 9])),
    );
}

#[test]
fn test_full_certificate_budget_admits_broader_and_complementary_proofs() {
    let fixture = Fixture::new();
    let block = Block::new_unique(SLOT);
    let notar = Vote::new_notarization_vote(block);
    let skip = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    for ranks in [&[0, 1, 2, 3], &[0, 1, 2, 4], &[0, 1, 2, 5], &[0, 1, 3, 4]] {
        recovery.add_certificate(fixture.certificate(notar, ranks), &fixture.rank_map);
    }
    recovery.add_certificate(
        fixture.certificate(notar, &[0, 1, 2, 3, 4]),
        &fixture.rank_map,
    );
    assert_eq!(recovery.metrics.replaced_certificates, 1);
    let broader = recovery.slots[&SLOT].payloads[&notar]
        .seeds
        .iter()
        .any(|seed| seed.ranks == fixture.ranks(&[0, 1, 2, 3, 4]));
    assert!(broader);
    recovery.add_certificate(fixture.certificate(skip, &[2, 3, 4, 5]), &fixture.rank_map);
    assert_eq!(recovery.metrics.replaced_certificates, 2);
    assert_eq!(
        recovery.slots[&SLOT].num_certificates,
        MAX_CERTIFICATES_PER_SLOT
    );
    wait_until_prepared(&mut recovery);
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    let Decoded::Base2(ranks) = decode(
        response.notar.as_ref().unwrap().bitmap(),
        fixture.rank_map.len(),
    )
    .unwrap() else {
        panic!("expected base2 notar output");
    };
    let notar_ranks: Vec<_> = ranks.iter_ones().collect();
    assert_eq!(response.validators.len(), NUM_VALIDATORS);
    fixture.assert_response(
        &response,
        Some(&[2, 3, 4, 5]),
        Some((block.block_id, &notar_ranks)),
    );
}

#[test]
fn test_raw_payload_pressure_cannot_exclude_a_verified_certificate() {
    let fixture = Fixture::new();
    let mut recovery = Recovery::new();
    for _ in 0..MAX_PAYLOADS_PER_SLOT {
        let vote = Vote::new_notarization_vote(Block::new_unique(SLOT));
        recovery.add_signatures(vote, fixture.signatures(vote, &[0]), &fixture.rank_map);
    }
    let skip = Vote::new_skip_vote(SLOT);
    recovery.add_certificate(fixture.certificate(skip, &[0, 1, 2, 3]), &fixture.rank_map);
    assert_eq!(recovery.slots[&SLOT].payloads.len(), MAX_PAYLOADS_PER_SLOT);
    assert_eq!(recovery.metrics.dropped_payloads, 1);
    wait_until_prepared(&mut recovery);
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
}

#[test]
fn test_prepared_frontier_is_bounded_and_keeps_complementary_signer_coverage() {
    let fixture = Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), 12);
    let block = Block::new_unique(SLOT);
    let notar = Vote::new_notarization_vote(block);
    let mut ready = vec![];
    retain_ready(
        &mut ready,
        CheckedCandidate::verify(
            Candidate::from_certificate(
                fixture.certificate(notar, &(4..12).collect::<Vec<_>>()),
                &fixture.rank_map,
            )
            .unwrap(),
            &fixture.rank_map,
            SHRED_VERSION,
        )
        .unwrap(),
        &fixture.rank_map,
    );
    for left in 7..12 {
        for right in left + 1..12 {
            let ranks: Vec<_> = (0..6).chain([left, right]).collect();
            retain_ready(
                &mut ready,
                CheckedCandidate::verify(
                    Candidate::from_certificate(
                        fixture.certificate(notar, &ranks),
                        &fixture.rank_map,
                    )
                    .unwrap(),
                    &fixture.rank_map,
                    SHRED_VERSION,
                )
                .unwrap(),
                &fixture.rank_map,
            );
            assert!(ready.len() <= MAX_READY_CANDIDATES_PER_PAYLOAD);
        }
    }
    assert_eq!(ready.len(), MAX_READY_CANDIDATES_PER_PAYLOAD);
    let mut recovery = Recovery::new();
    recovery.add_signatures(notar, [], &fixture.rank_map);
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&notar)
        .unwrap()
        .ready = ready;
    let response = recovery.finish(
        SLOT,
        SHRED_VERSION,
        fixture.response(Vote::new_skip_vote(SLOT), &[0, 1, 2, 3]),
    );
    fixture.assert_response(
        &response,
        Some(&[0, 1, 2, 3]),
        Some((block.block_id, &(4..12).collect::<Vec<_>>())),
    );
}

#[test]
fn test_cached_coverage_scores_match_direct_scoring_for_every_retained_subset() {
    fn direct_score(candidates: &[&Candidate], rank_map: &BLSPubkeyToRankMap) -> (u64, u64, u64) {
        let mut best = 0;
        let mut ranks = BitVec::<u8>::repeat(false, rank_map.len());
        let mut total = 0u64;
        for &candidate in candidates {
            best = best.max(candidate.stake);
            total = total.saturating_add(candidate.stake);
            ranks |= &candidate.ranks;
            if matches!(candidate.vote, Vote::Skip(_)) {
                for &notar in candidates {
                    if matches!(notar.vote, Vote::Notarize(_)) {
                        best = best.max(union_stake(candidate, notar, rank_map));
                    }
                }
            }
        }
        let covered = ranks.iter_ones().fold(0u64, |stake, rank| {
            stake.saturating_add(
                rank_map
                    .get_pubkey_stake_entry(rank)
                    .unwrap()
                    .stake
                    .get()
                    .get(),
            )
        });
        (best, covered, total)
    }

    let fixture = Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), 12);
    let skip = Vote::new_skip_vote(SLOT);
    let notar_a = Vote::new_notarization_vote(Block::new_unique(SLOT));
    let notar_b = Vote::new_notarization_vote(Block::new_unique(SLOT));
    let incoming = Vote::new_notarization_vote(Block::new_unique(SLOT));
    let candidates = [
        (skip, (0..8).collect()),
        (skip, (4..12).collect()),
        (skip, vec![1, 3, 5, 7, 9, 11]),
        (notar_a, vec![0, 1, 2, 8, 9, 10, 11]),
        (notar_a, vec![3, 4, 5, 6, 7, 8, 9]),
        // A seed and a prepared candidate may cover the same ranks. The final quality
        // component must preserve their existing separate contributions.
        (notar_a, vec![0, 1, 2, 8, 9, 10, 11]),
        (notar_b, vec![0, 2, 4, 6, 8, 10]),
        (notar_b, vec![1, 3, 5, 7, 9, 11]),
        (incoming, (0..11).collect()),
    ]
    .into_iter()
    .map(|(vote, ranks): (_, Vec<usize>)| {
        Candidate::from_certificate(fixture.certificate(vote, &ranks), &fixture.rank_map).unwrap()
    })
    .collect::<Vec<_>>();
    let quality = CoverageQuality::new(candidates.iter().collect(), &fixture.rank_map);
    assert_eq!(quality.pair_stakes.len(), 3 * 6);
    // This includes all payload removals, seed removals within the same payload, and ready
    // frontier trims, including the empty set and plans retaining the incoming candidate.
    for mask in 0..1usize << candidates.len() {
        let kept: Vec<_> = (0..candidates.len())
            .filter(|index| mask & (1 << index) != 0)
            .collect();
        let direct = kept
            .iter()
            .map(|&index| &candidates[index])
            .collect::<Vec<_>>();
        assert_eq!(
            quality.score(kept),
            direct_score(&direct, &fixture.rank_map)
        );
    }
}
