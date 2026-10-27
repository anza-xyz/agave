use {
    super::{super::CertsBuilder, *},
    crate::block_creation_loop::rewards::{
        msg_types::RewardRequest,
        reward_certs_service::{Context, run_with_recovery_for_tests},
    },
    agave_bls_sigverify::{rewards::RewardInput, sig_verified_messages::VoteAggregate},
    agave_votor_messages::{
        certificate::CertificateType,
        consensus_message::{Block, BlockId, VoteMessage},
        reward_certificate::NUM_SLOTS_FOR_REWARD,
    },
    rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom},
    solana_bls_signatures::{Keypair as BlsKeypair, PubkeyCompressed, Scalar, Signature},
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::Keypair,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, MAX_ALPENGLOW_VOTE_ACCOUNTS, SlotLeader},
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

    fn vote_aggregate(&self, vote: Vote, ranks: &[usize]) -> VoteAggregate {
        VoteAggregate::new_from_verified_votes(
            self.rank_map.len(),
            agave_votor_messages::wire::VotePayloadToSign::new_from_vote(vote, SHRED_VERSION),
            ranks.iter().map(|&rank| {
                (
                    rank as u16,
                    self.rank_map.get_pubkey_stake_entry(rank).unwrap().stake,
                )
            }),
            SignatureProjective::try_from(&self.aggregate(vote, ranks)).unwrap(),
        )
    }

    fn add_votes(&self, recovery: &mut Recovery, vote: Vote, ranks: &[usize]) {
        for &rank in ranks {
            recovery.add_aggregate(self.vote_aggregate(vote, &[rank]), &self.rank_map);
        }
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
                assert_eq!(notar.block_id, block_id);
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

fn cluster_info() -> Arc<ClusterInfo> {
    let keypair = Arc::new(Keypair::new());
    let mut contact = ContactInfo::new_localhost(&keypair.pubkey(), 0);
    contact.set_shred_version(SHRED_VERSION);
    Arc::new(ClusterInfo::new(
        contact,
        keypair,
        SocketAddrSpace::Unspecified,
    ))
}

/// Drives the production routing while recovery runs on its controller thread.
fn service_context(fixture: &Fixture) -> (Context, Receiver<Slot>) {
    let builder = CertsBuilder::new();
    let cluster_info = cluster_info();
    let mut recovery = Recovery::new();
    let prepared = recovery.observe_completions();
    let (_, inputs) = bounded(1);
    let (_, requests) = bounded(1);
    let banks = fixture.bank_forks.read().unwrap().sharable_banks();
    (
        Context::new_with_gates(
            Arc::new(AtomicBool::new(false)),
            cluster_info,
            inputs,
            requests,
            banks,
            builder,
            recovery,
            None,
            None,
        ),
        prepared,
    )
}

fn build_from_service(context: &mut Context, source_slot: Slot) -> RewardRespSucc {
    let (reply_sender, reply_receiver) = bounded(1);
    context
        .build_request(Ok(RewardRequest {
            bank_slot: source_slot + NUM_SLOTS_FOR_REWARD,
            reply_sender,
            request_sent: Instant::now(),
        }))
        .unwrap();
    let response = reply_receiver.try_recv().unwrap();
    response
        .recovery
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|error| match error {
            crossbeam_channel::RecvTimeoutError::Disconnected => response.result.unwrap(),
            crossbeam_channel::RecvTimeoutError::Timeout => panic!("recovery did not finish"),
        })
}

#[test]
fn test_service_loop_schedules_and_collects_jobs_without_further_input() {
    let fixture = Fixture::new();
    let sources = [SLOT, SLOT + 1];
    let builder = CertsBuilder::new();
    let cluster_info = cluster_info();
    // A gated worker holds the first job while the second source's evidence arrives, so only the
    // idle controller loop can schedule the second job.
    let (jobs_sender, jobs_receiver) = bounded::<Job>(1);
    let (results_sender, results_receiver) = bounded(1);
    let (release_sender, release_receiver) = bounded::<()>(0);
    let worker = thread::spawn(move || {
        while let Ok(job) = jobs_receiver.recv() {
            if release_receiver.recv().is_err() {
                return;
            }
            if results_sender.send(job.run()).is_err() {
                return;
            }
        }
    });
    let mut recovery = Recovery::from_worker(jobs_sender, results_receiver, worker);
    let completed = recovery.observe_completions();
    let (input, input_receiver) = bounded(4);
    for slot in sources {
        let vote = Vote::new_skip_vote(slot);
        for input_value in [
            RewardInput::External(vec![
                fixture.vote_aggregate(vote, &[3]),
                fixture.vote_aggregate(vote, &[4]),
            ]),
            RewardInput::Certificates(vec![fixture.certificate(vote, &[0, 1, 2, 3])]),
        ] {
            input.send(input_value).unwrap();
        }
    }
    let exit = Arc::new(AtomicBool::new(false));
    let (requests, request_receiver) = bounded(sources.len());
    let sharable_banks = fixture.bank_forks.read().unwrap().sharable_banks();
    let service = thread::spawn({
        let exit = exit.clone();
        move || {
            run_with_recovery_for_tests(
                exit,
                cluster_info,
                input_receiver,
                request_receiver,
                sharable_banks,
                builder,
                recovery,
                None,
            )
        }
    });
    for slot in sources {
        release_sender
            .send_timeout((), Duration::from_secs(10))
            .expect("the worker received a job");
        assert_eq!(completed.recv_timeout(Duration::from_secs(10)), Ok(slot));
    }
    // No more evidence arrives. Only the idle controller collects results and schedules work.
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
        let response = response
            .recovery
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        fixture.assert_response_at(slot, &response, Some(&[0, 1, 2, 3, 4]), None);
    }
    exit.store(true, Ordering::Relaxed);
    drop(input);
    service.join().unwrap();
}

#[test]
fn test_service_extends_a_certificate_with_external_vote_aggregates() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let (mut context, prepared) = service_context(&fixture);
    context.handle_input(RewardInput::External(vec![
        fixture.vote_aggregate(vote, &[4, 5]),
    ]));
    context.handle_input(RewardInput::Certificates(vec![
        fixture.certificate(vote, &[0, 1, 2, 3]),
    ]));
    assert_eq!(
        prepared.recv_timeout(Duration::from_secs(10)).unwrap(),
        SLOT
    );
    let response = build_from_service(&mut context, SLOT);
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4, 5]), None);
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
    assert!(
        recovery.slots[&SLOT].payloads[&vote].seeds[0]
            .checked()
            .is_some()
    );
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
    assert_eq!(
        state.seeds.iter().filter_map(Seed::checked).count(),
        MAX_CERTIFICATES_PER_SLOT - 1
    );
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
            .unwrap()
            .into(),
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
        recovery.add_aggregate(fixture.vote_aggregate(vote, &[0]), rank_map);
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
        recovery.slots[&SLOT].payloads[&vote].seeds[0].check,
        SeedCheck::Invalid { .. }
    ));
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.checks, 3);
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
}

#[test]
fn test_rejected_seed_keeps_vote_aggregates_and_cannot_spin() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut certificate = fixture.certificate(vote, &[0, 1, 2, 3]);
    certificate.signature = fixture.aggregate(vote, &[0]);
    let mut recovery = Recovery::new();
    recovery.add_certificate(certificate, &fixture.rank_map);
    fixture.add_votes(&mut recovery, vote, &[4, 5]);
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.checks, 1);
    assert_eq!(recovery.metrics.unverified_outputs, 1);
    let state = &recovery.slots[&SLOT].payloads[&vote];
    assert_eq!(state.aggregates.len(), 2);
    assert!(state.ready.is_empty());
    for _ in 0..4 {
        recovery.poll(SHRED_VERSION);
    }
    assert_eq!(recovery.metrics.completed_jobs, 1);
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[4, 5]));
    fixture.assert_response(&response, Some(&[4, 5]), None);
}

#[test]
fn test_seed_preparation_and_extensions_share_the_bounded_job_budget() {
    let fixture = Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), 24);
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    for rank in 0..MAX_CERTIFICATES_PER_SLOT {
        recovery.add_certificate(fixture.certificate(vote, &[rank]), &fixture.rank_map);
    }
    let outside: Vec<_> = (MAX_CERTIFICATES_PER_SLOT..24).collect();
    recovery.add_aggregate(fixture.vote_aggregate(vote, &outside), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.checks, MAX_CHECKS_PER_JOB);
    assert_eq!(recovery.metrics.failed_checks, 0);
    assert_eq!(recovery.metrics.completed_jobs, 1);
    assert_eq!(
        recovery.slots[&SLOT].payloads[&vote]
            .seeds
            .iter()
            .filter_map(Seed::checked)
            .count(),
        MAX_CERTIFICATES_PER_SLOT
    );
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    assert_eq!(response.validators.len(), outside.len() + 1);
    let Decoded::Base2(ranks) = decode(
        response.skip.as_ref().unwrap().to_bitmap(),
        fixture.rank_map.len(),
    )
    .unwrap() else {
        panic!("expected base2 output");
    };
    fixture.assert_response(
        &response,
        Some(&ranks.iter_ones().collect::<Vec<_>>()),
        None,
    );
    assert_eq!(recovery.metrics.checks, MAX_CHECKS_PER_JOB);
}

#[test]
fn test_certificate_and_disjoint_vote_aggregates_merge_in_either_arrival_order() {
    let fixture = Fixture::new();
    let block_id = BlockId::new_unique();
    let vote = Vote::new_notarization_vote(Block {
        slot: SLOT,
        block_id,
    });
    for certificate_first in [false, true] {
        let mut recovery = Recovery::new();
        let cert = fixture.certificate(vote, &[0, 1, 2, 3]);
        if certificate_first {
            recovery.add_certificate(cert.clone(), &fixture.rank_map);
            recovery.poll(SHRED_VERSION);
        }
        // One complete batch is inside the certificate and the other is outside it.
        for ranks in [[2, 3], [4, 5]] {
            recovery.add_aggregate(fixture.vote_aggregate(vote, &ranks), &fixture.rank_map);
        }
        if !certificate_first {
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
fn test_smaller_certificate_extends_an_aggregate_that_overlaps_a_larger_certificate() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(
        fixture.certificate(vote, &[0, 1, 2, 3, 4]),
        &fixture.rank_map,
    );
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2, 3]), &fixture.rank_map);
    recovery.add_aggregate(fixture.vote_aggregate(vote, &[4, 5]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[4, 5]));
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4, 5]), None);
    assert_eq!(recovery.metrics.recovered_stake, 400);
    assert_eq!(recovery.metrics.failed_checks, 0);
}

#[test]
fn test_overlapping_vote_aggregates_remain_whole_alternatives() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    for ranks in [&[0, 1][..], &[1, 2][..], &[3][..]] {
        recovery.add_aggregate(fixture.vote_aggregate(vote, ranks), &fixture.rank_map);
    }
    recovery.add_certificate(fixture.certificate(vote, &[0, 4, 5]), &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.failed_checks, 0);
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[0, 1, 2, 3]));
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4, 5]), None);
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

    // An extension with rank 3's vote aggregate keeps it, so it replaces the baseline.
    let mut recovery = Recovery::new();
    recovery.add_certificate(certificate(), &fixture.rank_map);
    fixture.add_votes(&mut recovery, vote, &[3]);
    wait_for_check(&mut recovery);
    wait_until_prepared(&mut recovery);
    let response = recovery.finish(SLOT, SHRED_VERSION, baseline());
    fixture.assert_response(&response, None, Some((block_id, &[0, 1, 2, 3, 4, 5])));
    assert_eq!(recovery.metrics.recovered_stake, 200);
}

#[test]
fn test_optimistic_aggregate_is_never_split_at_a_certificate_boundary() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut signatures = fixture.signatures(vote, &[0, 1, 2, 3, 4]);
    let delta: Signature = BlsKeypair::new().sign(b"perturbation").into();
    for (index, weight) in [(0, Scalar::ONE), (4, -Scalar::ONE)] {
        signatures[index].1 = SignatureProjective::aggregate_with_weights(
            [&signatures[index].1, &delta].into_iter(),
            [Scalar::ONE, weight].iter(),
        )
        .unwrap()
        .into();
    }
    let direct = VoteAggregate::new_from_verified_votes(
        fixture.rank_map.len(),
        agave_votor_messages::wire::VotePayloadToSign::new_from_vote(vote, SHRED_VERSION),
        (0..5).map(|rank| {
            (
                rank as u16,
                fixture.rank_map.get_pubkey_stake_entry(rank).unwrap().stake,
            )
        }),
        SignatureProjective::aggregate(signatures.iter().map(|(_, signature)| signature)).unwrap(),
    );
    assert_eq!(direct, fixture.vote_aggregate(vote, &[0, 1, 2, 3, 4]));
    for (seed, expected) in [
        (&[0, 1, 2, 3][..], &[0, 1, 2, 3, 4][..]),
        (&[5][..], &[0, 1, 2, 3, 4, 5][..]),
    ] {
        let mut recovery = Recovery::new();
        recovery.add_certificate(fixture.certificate(vote, seed), &fixture.rank_map);
        recovery.add_aggregate(direct.clone(), &fixture.rank_map);
        wait_until_prepared(&mut recovery);
        assert_eq!(recovery.metrics.failed_checks, 0);
        assert_eq!(
            recovery.slots[&SLOT].payloads[&vote]
                .aggregates
                .iter()
                .map(Arc::as_ref)
                .collect::<Vec<_>>(),
            vec![&direct]
        );
        let response = recovery.finish(
            SLOT,
            SHRED_VERSION,
            fixture.response(vote, &[0, 1, 2, 3, 4]),
        );
        fixture.assert_response(&response, Some(expected), None);
    }
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
fn test_vote_aggregate_storage_is_bounded_and_ignores_duplicate_coverage() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    fixture.add_votes(&mut recovery, vote, &[0, 1, 2, 3, 4, 5]);
    let generation = recovery.slots[&SLOT].payloads[&vote].generation;
    recovery.add_aggregate(fixture.vote_aggregate(vote, &[0, 1]), &fixture.rank_map);
    let mismatched = VoteAggregate::new_from_verified_vote(
        NUM_VALIDATORS + 1,
        VoteMessage {
            vote,
            signature: *fixture.vote_aggregate(vote, &[0]).signature(),
            rank: 0,
            stake: fixture.rank_map.get_pubkey_stake_entry(0).unwrap().stake,
        },
    );
    recovery.add_aggregate(mismatched, &fixture.rank_map);
    let state = &recovery.slots[&SLOT].payloads[&vote];
    assert_eq!(state.aggregates.len(), fixture.rank_map.len());
    assert_eq!(*state.aggregates[0], fixture.vote_aggregate(vote, &[0]));
    assert_eq!(state.generation, generation);
    assert_eq!(recovery.metrics.invalid_vote_aggregates, 1);
    for _ in 0..MAX_PAYLOADS_PER_SLOT + 2 {
        let vote = Vote::new_notarization_vote(Block::new_unique(SLOT));
        recovery.add_aggregate(fixture.vote_aggregate(vote, &[0]), &fixture.rank_map);
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
        fixture.add_votes(&mut recovery, vote, &[4, 5]);
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
fn test_service_accepts_finalized_source_slot_using_its_epoch_rank_map() {
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
    fixture.bank_forks.write().unwrap().insert(root_bank);
    fixture
        .bank_forks
        .write()
        .unwrap()
        .set_root(130, None, None);
    let (mut context, prepared) = service_context(&fixture);
    context.handle_input(RewardInput::Certificates(vec![
        fixture.certificate(vote, &[0, 1, 2, 3]),
    ]));
    assert_eq!(
        prepared.recv_timeout(Duration::from_secs(10)).unwrap(),
        source_slot
    );
    let response = build_from_service(&mut context, source_slot);
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
}

#[test]
fn test_service_rejects_late_evidence_for_sealed_slot_even_while_root_lags() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let (mut context, prepared) = service_context(&fixture);
    context.metrics.last_report = Instant::now().checked_add(Duration::from_secs(60)).unwrap();
    context.handle_input(RewardInput::Certificates(vec![
        fixture.certificate(vote, &[0, 1, 2, 3]),
    ]));
    assert_eq!(
        prepared.recv_timeout(Duration::from_secs(10)).unwrap(),
        SLOT
    );
    let response = build_from_service(&mut context, SLOT);
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
    assert_eq!(fixture.bank.slot(), SLOT);
    let own_vote = VoteMessage {
        vote,
        signature: fixture.signatures(vote, &[5])[0].1.try_into().unwrap(),
        rank: 5,
        stake: fixture.rank_map.get_pubkey_stake_entry(5).unwrap().stake,
    };
    for input in [
        RewardInput::Certificates(vec![fixture.certificate(vote, &[2, 3, 4, 5])]),
        RewardInput::External(vec![fixture.vote_aggregate(vote, &[4])]),
        RewardInput::Own(own_vote),
    ] {
        context.handle_input(input);
        assert!(!context.builder.aggregates.contains_key(&SLOT));
    }
    assert_eq!(context.metrics.late_vote_aggregates, 2);
    assert_eq!(
        build_from_service(&mut context, SLOT + 1),
        RewardRespSucc::default()
    );
    let (_, recovery) = context.into_parts();
    assert!(!recovery.slots.contains_key(&SLOT));
}

#[test]
fn test_recovery_uses_verified_ready_candidate_while_newer_worker_job_is_blocked() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let certificate = fixture.certificate(vote, &[0, 1, 2, 3]);
    let mut ready = Job {
        vote,
        generation: 1,
        rank_map: fixture.rank_map.clone(),
        shred_version: SHRED_VERSION,
        seeds: vec![Seed::new(
            Candidate::from_certificate(certificate.clone(), &fixture.rank_map).unwrap(),
        )],
        verification_gate: None,
        aggregates: vec![Arc::new(fixture.vote_aggregate(vote, &[4]))],
        ready: vec![],
    }
    .run()
    .candidates;
    let ready = ready.pop().unwrap();
    let (jobs_sender, jobs_receiver) = bounded::<Job>(1);
    let (results_sender, results_receiver) = bounded(1);
    let (started_sender, started_receiver) = bounded(1);
    // Drop the release sender before recovery on panic, so its worker can exit before joining.
    let mut recovery;
    let (release_sender, release_receiver) = bounded(1);
    let worker = thread::spawn(move || {
        if let Ok(job) = jobs_receiver.recv() {
            started_sender.send(()).unwrap();
            let _ = release_receiver.recv();
            let _ = results_sender.send(job.run());
        }
    });
    recovery = Recovery::from_worker(jobs_sender, results_receiver, worker);
    recovery.add_certificate(certificate, &fixture.rank_map);
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&vote)
        .unwrap()
        .ready = vec![ready];
    recovery.add_certificate(fixture.certificate(vote, &[1, 2, 3, 5]), &fixture.rank_map);
    recovery.poll(SHRED_VERSION);
    started_receiver
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    assert!(recovery.in_flight);
    release_sender.send(()).unwrap();
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4]), None);
    wait_for_check(&mut recovery);
    assert!(!recovery.slots.contains_key(&SLOT));
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
        fixture.add_votes(&mut recovery, vote, &[rank]);
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
    fixture.add_votes(&mut recovery, vote, &[43]);
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
    fixture.add_votes(&mut recovery, hot, &[4]);
    wait_for_check(&mut recovery);
    fixture.add_votes(&mut recovery, hot, &[5]);
    recovery
        .slots
        .get_mut(&SLOT)
        .unwrap()
        .payloads
        .get_mut(&hot)
        .unwrap()
        .last_check = Some(Instant::now().checked_sub(CHECK_INTERVAL).unwrap());
    recovery.add_certificate(fixture.certificate(new, &[0, 1, 2, 3]), &fixture.rank_map);
    fixture.add_votes(&mut recovery, new, &[4]);
    recovery.poll(SHRED_VERSION);
    let hot_state = &recovery.slots[&SLOT].payloads[&hot];
    assert_ne!(hot_state.scheduled_generation, hot_state.generation);
    let new_state = &recovery.slots[&(SLOT + 1)].payloads[&new];
    assert_eq!(new_state.scheduled_generation, new_state.generation);
    wait_for_completed_jobs(&mut recovery, 2);
    assert!(!recovery.slots[&(SLOT + 1)].payloads[&new].ready.is_empty());
}

#[test]
fn test_randomized_recovery_keeps_baseline_signers_with_overlapping_aggregates() {
    const VALIDATORS: usize = 24;
    let fixture =
        Fixture::new_with_num_validators(SLOT, EpochSchedule::without_warmup(), VALIDATORS);
    let vote = Vote::new_skip_vote(SLOT);
    let mut rng = StdRng::seed_from_u64(42);
    for _ in 0..24 {
        let mut shuffled: Vec<_> = (0..VALIDATORS).collect();
        shuffled.shuffle(&mut rng);
        let direct = &shuffled[..VALIDATORS / 2];
        let mut recovery = Recovery::new();
        for chunk in direct.chunks(rng.random_range(1..=4)) {
            let aggregate = fixture.vote_aggregate(vote, chunk);
            recovery.add_aggregate(aggregate.clone(), &fixture.rank_map);
            recovery.add_aggregate(aggregate, &fixture.rank_map);
        }
        shuffled.shuffle(&mut rng);
        recovery.add_certificate(
            fixture.certificate(vote, &shuffled[..16]),
            &fixture.rank_map,
        );
        wait_until_prepared(&mut recovery);
        assert_eq!(recovery.metrics.failed_checks, 0);
        assert_eq!(recovery.metrics.unverified_outputs, 0);
        let direct = recovery.slots[&SLOT].payloads[&vote]
            .aggregates
            .iter()
            .flat_map(|aggregate| aggregate.ranks().iter_ones())
            .collect::<Vec<_>>();
        let baseline = fixture.response(vote, &direct);
        let response = recovery.finish(SLOT, SHRED_VERSION, baseline);
        let Decoded::Base2(ranks) =
            decode(response.skip.as_ref().unwrap().to_bitmap(), VALIDATORS).unwrap()
        else {
            panic!("expected base2 output");
        };
        assert!(direct.iter().all(|&rank| ranks[rank]));
        fixture.assert_response(
            &response,
            Some(&ranks.iter_ones().collect::<Vec<_>>()),
            None,
        );
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
    fixture.add_votes(&mut recovery, vote, &[4]);
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
    fixture.add_votes(&mut recovery, vote, &[5]);
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
    fixture.add_votes(&mut recovery, vote, &[5]);
    recovery.poll(SHRED_VERSION);
    assert!(!recovery.in_flight);
    assert_eq!(recovery.poll_interval(), Duration::from_secs(1));
    assert_eq!(recovery.metrics.worker_exits, 1);
    // The candidate prepared before the exit is still published.
    let response = recovery.finish(SLOT, SHRED_VERSION, RewardRespSucc::default());
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4]), None);
}

#[test]
fn test_vote_payloads_for_made_up_blocks_cannot_keep_out_stronger_payloads() {
    let fixture = Fixture::new();
    let mut recovery = Recovery::new();
    // Each colluder's own vote for a made-up block opens a payload with one vote aggregate.
    for rank in 0..MAX_PAYLOADS_PER_SLOT {
        let made_up = Vote::new_notarization_vote(Block::new_unique(SLOT));
        fixture.add_votes(&mut recovery, made_up, &[rank]);
    }
    let skip = Vote::new_skip_vote(SLOT);
    recovery.add_aggregate(fixture.vote_aggregate(skip, &[4, 5]), &fixture.rank_map);
    assert!(recovery.slots[&SLOT].payloads.contains_key(&skip));
    assert_eq!(recovery.slots[&SLOT].payloads.len(), MAX_PAYLOADS_PER_SLOT);
    assert_eq!(recovery.metrics.dropped_payloads, 1);
    // A payload no stronger than the weakest one does not displace it.
    let weak = Vote::new_notarization_vote(Block::new_unique(SLOT));
    fixture.add_votes(&mut recovery, weak, &[5]);
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
    let (mut context, prepared) = service_context(&fixture);
    let own_vote = VoteMessage {
        vote,
        signature: fixture.signatures(vote, &[4])[0].1.try_into().unwrap(),
        rank: 4,
        stake: fixture.rank_map.get_pubkey_stake_entry(4).unwrap().stake,
    };
    context.handle_input(RewardInput::Own(own_vote));
    context.handle_input(RewardInput::Certificates(vec![
        fixture.certificate(vote, &[0, 1, 2, 3]),
    ]));
    assert_eq!(
        prepared.recv_timeout(Duration::from_secs(10)).unwrap(),
        SLOT
    );
    let response = build_from_service(&mut context, SLOT);
    fixture.assert_response(&response, Some(&[0, 1, 2, 3, 4]), None);
}

#[test]
fn test_unchanged_prepared_extension_is_not_reverified() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2]), &fixture.rank_map);
    recovery.add_certificate(fixture.certificate(vote, &[3, 4, 5]), &fixture.rank_map);
    fixture.add_votes(&mut recovery, vote, &[0, 3]);
    wait_for_check(&mut recovery);
    assert_eq!(recovery.metrics.checks, 4);
    // Rank 1 extends only the second seed. The first seed's prospective set is unchanged and
    // already prepared, and the larger new extension does not cover it.
    fixture.add_votes(&mut recovery, vote, &[1]);
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
    // Only the smaller seed misses these aggregates, and the larger seed covers its extension.
    fixture.add_votes(&mut recovery, vote, &[3, 4]);
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
fn test_failed_extension_never_relaxes_the_baseline_signer_floor() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(
        fixture.certificate(vote, &[0, 1, 2, 4, 5]),
        &fixture.rank_map,
    );
    // Break the ingress invariant to ensure even a failed replay check cannot remove baseline payees.
    let wrong_vote = Vote::new_skip_vote(SLOT + 1);
    let mismatched = VoteAggregate::new_from_verified_vote(
        NUM_VALIDATORS,
        VoteMessage {
            vote,
            signature: *fixture.vote_aggregate(wrong_vote, &[3]).signature(),
            rank: 3,
            stake: fixture.rank_map.get_pubkey_stake_entry(3).unwrap().stake,
        },
    );
    recovery.add_aggregate(mismatched, &fixture.rank_map);
    wait_until_prepared(&mut recovery);
    assert_eq!(recovery.metrics.failed_checks, 1);
    let response = recovery.finish(SLOT, SHRED_VERSION, fixture.response(vote, &[0, 1, 2, 3]));
    fixture.assert_response(&response, Some(&[0, 1, 2, 3]), None);
}

#[test]
fn test_aggregates_inside_every_seed_schedule_no_job() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut recovery = Recovery::new();
    recovery.add_certificate(fixture.certificate(vote, &[0, 1, 2]), &fixture.rank_map);
    fixture.add_votes(&mut recovery, vote, &[0, 1]);
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
    fixture.add_votes(&mut recovery, notar, &[9]);
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
fn test_vote_payload_pressure_cannot_exclude_a_verified_certificate() {
    let fixture = Fixture::new();
    let mut recovery = Recovery::new();
    for _ in 0..MAX_PAYLOADS_PER_SLOT {
        let vote = Vote::new_notarization_vote(Block::new_unique(SLOT));
        fixture.add_votes(&mut recovery, vote, &[0]);
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
            .unwrap()
            .into(),
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
                    .unwrap()
                    .into(),
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
    recovery.add_aggregate(fixture.vote_aggregate(notar, &[0]), &fixture.rank_map);
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
