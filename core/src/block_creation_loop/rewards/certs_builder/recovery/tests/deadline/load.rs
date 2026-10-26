//! Optimized, explicitly invoked measurements of the real service and production requestor.
//! Prepared historical frontiers are verified before timing; background bisection runs while cap
//! admissions are queued alongside the measured requests. With certificate evidence, the first
//! request arrives while the recovery controller has already dispatched a cap admission. Baseline
//! readiness is timed separately from the block footer's fixed observation window. Shared-host
//! timings are observations, not hard production latency guarantees.
use {
    super::*,
    crate::block_creation_loop::rewards::reward_certs_service::run_with_builder_and_admission_gate_for_tests,
    std::sync::Mutex,
};

const SOURCES: usize = 8;
const ROUNDS: usize = 4;
// No available pair covers this signer, so selection must score the complete frontier even if
// an implementation can stop early when it finds a pair covering every validator.
const OMITTED_RANK: usize = 0;

type PreparedPayload = (
    Vote,
    Candidate,
    Vec<CheckedCandidate>,
    BTreeMap<u16, Signature>,
);
type JobMeasurements = Arc<Mutex<Vec<(u64, u64)>>>;

struct Source {
    slot: Slot,
    baseline: VoteAggregate,
    payloads: Vec<PreparedPayload>,
    cap: Certificate,
}

fn aggregate(signatures: &[(u16, Signature)], ranks: &[usize]) -> SignatureCompressed {
    SignatureProjective::aggregate(ranks.iter().map(|&rank| &signatures[rank].1))
        .unwrap()
        .into()
}

fn candidate(
    fixture: &Fixture,
    vote: Vote,
    signatures: &[(u16, Signature)],
    ranks: &[usize],
) -> Candidate {
    Candidate::new(
        vote,
        aggregate(signatures, ranks),
        fixture.ranks(ranks),
        &fixture.rank_map,
    )
    .unwrap()
}

fn prepare(fixture: &Fixture) -> Vec<Source> {
    let n = fixture.rank_map.len();
    let all: Vec<_> = (0..n).collect();
    let direct: Vec<_> = (n / 2..n).collect();
    (0..SOURCES)
        .map(|offset| {
            let slot = SLOT + offset as Slot;
            let mut baseline = None;
            let payloads = (0..MAX_PAYLOADS_PER_SLOT)
                .map(|payload| {
                    let vote = if payload == 0 {
                        Vote::new_skip_vote(slot)
                    } else {
                        Vote::new_notarization_vote(Block {
                            slot,
                            block_id: BlockId::new_unique(),
                        })
                    };
                    let signatures = fixture.signatures(vote, &all);
                    if payload == 0 {
                        baseline = Some(VoteAggregate::new_from_verified_votes(
                            n,
                            agave_votor_messages::wire::VotePayloadToSign::new_from_vote(
                                vote,
                                SHRED_VERSION,
                            ),
                            direct.iter().map(|&rank| {
                                (
                                    rank as u16,
                                    fixture.rank_map.get_pubkey_stake_entry(rank).unwrap().stake,
                                )
                            }),
                            SignatureProjective::try_from(&aggregate(&signatures, &direct))
                                .unwrap(),
                        ));
                    }
                    let seed_ranks: Vec<_> = (0..n * 7 / 10)
                        .filter(|&rank| rank != OMITTED_RANK)
                        .collect();
                    let seed = candidate(fixture, vote, &signatures, &seed_ranks);
                    let ready = (0..MAX_READY_CANDIDATES_PER_PAYLOAD)
                        .map(|variant| {
                            let ranks: Vec<_> = (0..n)
                                .filter(|rank| {
                                    let stripe = rank % 10;
                                    *rank != OMITTED_RANK
                                        && stripe != variant
                                        && stripe != (variant + 1) % 10
                                })
                                .collect();
                            CheckedCandidate::verify(
                                candidate(fixture, vote, &signatures, &ranks),
                                &fixture.rank_map,
                                SHRED_VERSION,
                            )
                            .unwrap()
                        })
                        .collect();
                    let mut raw: BTreeMap<_, _> = signatures[n / 2..].iter().copied().collect();
                    // These pairs cancel within the direct batch. The seed covers one member of each
                    // pair, so its full extension fails and the worker must bisect the outside subset.
                    let delta: Signature =
                        BlsKeypair::new().sign(b"deadline bisection load").into();
                    for (inside, outside) in [(n / 2, n * 4 / 5), (n / 2 + 1, n * 9 / 10)] {
                        for (rank, weight) in [(inside, Scalar::ONE), (outside, -Scalar::ONE)] {
                            let original = raw.get_mut(&(rank as u16)).unwrap();
                            *original = SignatureProjective::aggregate_with_weights(
                                [&*original, &delta].into_iter(),
                                [Scalar::ONE, weight].iter(),
                            )
                            .unwrap()
                            .into();
                        }
                    }
                    (vote, seed, ready, raw)
                })
                .collect();
            let cap_vote = Vote::new_notarization_vote(Block {
                slot,
                block_id: BlockId::new_unique(),
            });
            let cap_signatures = fixture.signatures(cap_vote, &all);
            let cap_ranks: Vec<_> = (0..n * 9 / 10)
                .filter(|&rank| rank != OMITTED_RANK)
                .collect();
            Source {
                slot,
                baseline: baseline.unwrap(),
                payloads,
                cap: Certificate {
                    cert_type: CertificateType::Notarize(match cap_vote {
                        Vote::Notarize(notar) => notar.block,
                        _ => unreachable!(),
                    }),
                    signature: SignatureProjective::try_from(&aggregate(
                        &cap_signatures,
                        &cap_ranks,
                    ))
                    .unwrap()
                    .into(),
                    bitmap: encode_base2(&fixture.ranks(&cap_ranks)).unwrap(),
                },
            }
        })
        .collect()
}

fn service_builder(
    fixture: &Fixture,
    sources: &[Source],
    with_certificate_evidence: bool,
) -> (CertsBuilder, JobMeasurements, Option<Receiver<()>>) {
    let mut builder = builder();
    for source in sources {
        builder.handle_input(
            &fixture.bank,
            RewardInput::External(vec![source.baseline.clone()]),
        );
    }
    let measurements = Arc::new(Mutex::new(Vec::new()));
    if with_certificate_evidence {
        let (jobs, job_receiver) = bounded::<Job>(1);
        let (result_sender, results) = bounded(1);
        let (started_sender, started) = bounded(1);
        let worker_measurements = measurements.clone();
        let worker = thread::spawn(move || {
            while let Ok(job) = job_receiver.recv() {
                let _ = started_sender.try_send(());
                let result = job.run();
                worker_measurements
                    .lock()
                    .unwrap()
                    .push((result.checks, result.failed_checks));
                if result_sender.send(result).is_err() {
                    break;
                }
            }
        });
        let mut recovery = Recovery::from_worker(jobs, results, worker);
        for source in sources {
            let payloads = source
                .payloads
                .iter()
                .map(|(vote, seed, ready, raw)| {
                    (
                        *vote,
                        PayloadState {
                            seeds: vec![seed.clone()],
                            ready: ready.clone(),
                            signatures: raw.clone(),
                            generation: 1,
                            ..PayloadState::default()
                        },
                    )
                })
                .collect();
            recovery.slots.insert(
                source.slot,
                SlotState {
                    rank_map: fixture.rank_map.clone(),
                    payloads,
                    num_certificates: MAX_CERTIFICATES_PER_SLOT,
                },
            );
        }
        builder.recovery = Some(recovery);
        (builder, measurements, Some(started))
    } else {
        (builder, measurements, None)
    }
}

#[test]
#[ignore = "optimized max-frontier deadline load probe; run explicitly with --release --ignored \
            --nocapture"]
fn reward_recovery_production_deadline_load() {
    let fixture = Fixture::new_with_num_validators(
        SLOT,
        EpochSchedule::without_warmup(),
        MAX_ALPENGLOW_VOTE_ACCOUNTS,
    );
    let sources = prepare(&fixture);
    for budget in [
        Duration::from_millis(2),
        Duration::from_millis(10),
        Duration::from_millis(50),
    ] {
        for with_certificate_evidence in [false, true] {
            let mut latencies = Vec::new();
            let mut misses = 0;
            let mut checked_outputs = 0;
            let mut recovered_outputs = 0;
            let mut baseline_outputs = 0;
            let mut pending_upgrades = 0;
            let mut completed_jobs = 0;
            let mut failed_checks = 0;
            for _ in 0..ROUNDS {
                let (builder, jobs, started) =
                    service_builder(&fixture, &sources, with_certificate_evidence);
                let cluster_info = builder.cluster_info.clone();
                let my_pubkey = cluster_info.id();
                let banks = fixture.bank_forks.read().unwrap().sharable_banks();
                let exit = Arc::new(AtomicBool::new(false));
                let service_exit = exit.clone();
                let (input, inputs) = bounded(32);
                let (mut requestor, requests) = CertsRequestor::new();
                let (admission_started_sender, admission_started) = bounded(1);
                let (release_admission, admission_release) = bounded(1);
                let service = thread::spawn(move || {
                    run_with_builder_and_admission_gate_for_tests(
                        service_exit,
                        cluster_info,
                        inputs,
                        requests,
                        banks,
                        builder,
                        with_certificate_evidence
                            .then_some((admission_started_sender, admission_release)),
                    )
                });
                if let Some(started) = started {
                    started.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                // Queue a full fifth-payload batch, then wait until one certificate is dispatched.
                // The first request is queued while the service is committed to that admission.
                if with_certificate_evidence {
                    input
                        .send(RewardInput::Certificates(
                            (0..32)
                                .map(|index| sources[index % SOURCES].cap.clone())
                                .collect(),
                        ))
                        .unwrap();
                    admission_started
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
                let mut outputs = Vec::new();
                for source in &sources {
                    let mut metrics = SlotMetrics::new(source.slot + NUM_SLOTS_FOR_REWARD, true);
                    let started = Instant::now();
                    let request = requestor
                        .request_reward_certs(
                            &mut metrics,
                            my_pubkey,
                            source.slot + NUM_SLOTS_FOR_REWARD,
                        )
                        .unwrap()
                        .unwrap();
                    if with_certificate_evidence && source.slot == sources[0].slot {
                        release_admission.send(()).unwrap();
                    }
                    let mut select = Select::new();
                    select.recv(&request.reply_receiver);
                    let _ = select.ready_timeout(budget.saturating_sub(started.elapsed()));
                    drop(select);
                    latencies.push(started.elapsed().as_micros() as u64);
                    // Production observes once at footer construction, after its block work. Do
                    // not observe early merely because the direct baseline is already available:
                    // recovery has the same window to complete an upgrade, without holding it up.
                    thread::sleep(budget.saturating_sub(started.elapsed()));
                    let response = requestor
                        .recv_reward_certs(&mut metrics, my_pubkey, Some(request))
                        .unwrap();
                    misses += metrics.reward_certs_skipped;
                    pending_upgrades += metrics.reward_certs_recovery_pending;
                    if metrics.reward_certs_skipped == 0 {
                        outputs.push((source.slot, response));
                    }
                }
                exit.store(true, Ordering::Relaxed);
                drop(input);
                drop(requestor);
                drop(service.join().unwrap());
                let jobs = jobs.lock().unwrap();
                assert!(jobs.iter().all(|&(checks, _)| checks <= MAX_CHECKS_PER_JOB));
                completed_jobs += jobs.len();
                failed_checks += jobs.iter().map(|&(_, failures)| failures).sum::<u64>();
                // Validate exact signer metadata and signatures after timing, for every response
                // that the production try_recv actually accepted before its observation deadline.
                for (slot, response) in outputs {
                    if with_certificate_evidence {
                        let skip = response.skip.as_ref().map(|cert| {
                            match decode(cert.to_bitmap(), fixture.rank_map.len()).unwrap() {
                                Decoded::Base2(ranks) => ranks.iter_ones().collect::<Vec<_>>(),
                                _ => unreachable!(),
                            }
                        });
                        let notar = response.notar.as_ref().map(|cert| {
                            match decode(cert.bitmap(), fixture.rank_map.len()).unwrap() {
                                Decoded::Base2(ranks) => (
                                    BlockId::from(cert.block_id),
                                    ranks.iter_ones().collect::<Vec<_>>(),
                                ),
                                _ => unreachable!(),
                            }
                        });
                        fixture.assert_response_at(
                            slot,
                            &response,
                            skip.as_deref(),
                            notar.as_ref().map(|(id, ranks)| (*id, ranks.as_slice())),
                        );
                        if response.validators.len() == fixture.rank_map.len() - 1 {
                            recovered_outputs += 1;
                        } else {
                            assert_eq!(response.validators.len(), fixture.rank_map.len() / 2);
                            fixture.assert_response_at(
                                slot,
                                &response,
                                Some(
                                    &(fixture.rank_map.len() / 2..fixture.rank_map.len())
                                        .collect::<Vec<_>>(),
                                ),
                                None,
                            );
                            baseline_outputs += 1;
                        }
                    } else {
                        fixture.assert_response_at(
                            slot,
                            &response,
                            Some(
                                &(fixture.rank_map.len() / 2..fixture.rank_map.len())
                                    .collect::<Vec<_>>(),
                            ),
                            None,
                        );
                        baseline_outputs += 1;
                    }
                    checked_outputs += 1;
                }
            }
            latencies.sort_unstable();
            println!(
                "production_deadline_load validators={} sources={SOURCES} payloads=4 \
                 ready_per_payload=8 seeds_per_source=4 omitted_rank={OMITTED_RANK} \
                 queued_cap_certificates={} first_request_dispatched_cap={} \
                 with_certificate_evidence={with_certificate_evidence} budget_us={} requests={} \
                 misses={misses} baseline_ready_median_us={} baseline_ready_max_us={} \
                 verified_outputs={checked_outputs} recovered_outputs={recovered_outputs} \
                 baseline_outputs={baseline_outputs} pending_upgrades={pending_upgrades} \
                 completed_jobs={completed_jobs} failed_checks={failed_checks}",
                fixture.rank_map.len(),
                if with_certificate_evidence { 32 } else { 0 },
                usize::from(with_certificate_evidence),
                budget.as_micros(),
                latencies.len(),
                latencies[latencies.len() / 2],
                latencies.last().unwrap()
            );
            if with_certificate_evidence {
                assert!(
                    failed_checks > 0,
                    "background bisection load did not execute"
                );
            }
            if budget == Duration::from_millis(50) {
                assert_eq!(
                    misses, 0,
                    "representative request budget missed on this host"
                );
                if with_certificate_evidence {
                    assert_eq!(
                        recovered_outputs,
                        ROUNDS * SOURCES,
                        "recovery did not complete within the representative observation window"
                    );
                }
            }
        }
    }
}
