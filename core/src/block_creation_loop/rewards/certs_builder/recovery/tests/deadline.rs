use {
    super::*,
    crate::block_creation_loop::{rewards::certs_requestor::CertsRequestor, stats::SlotMetrics},
    crossbeam_channel::Select,
};

mod load;

/// Optional verification may be arbitrarily slow, but must not withhold an already assembled
/// direct baseline from the production requestor at its nonblocking response boundary.
#[test]
fn test_production_request_preserves_baseline_while_verification_is_blocked() {
    let fixture = Fixture::new();
    let vote = Vote::new_skip_vote(SLOT);
    let mut builder = builder();
    let aggregate = VoteAggregate::new_from_verified_votes(
        fixture.rank_map.len(),
        agave_votor_messages::wire::VotePayloadToSign::new_from_vote(vote, SHRED_VERSION),
        [0, 1].into_iter().map(|rank| {
            (
                rank as u16,
                fixture.rank_map.get_pubkey_stake_entry(rank).unwrap().stake,
            )
        }),
        SignatureProjective::aggregate(
            fixture
                .signatures(vote, &[0, 1])
                .iter()
                .map(|(_, signature)| signature),
        )
        .unwrap(),
    );
    builder.handle_input(&fixture.bank, RewardInput::External(vec![aggregate]));
    let (started_sender, started) = bounded(1);
    let (release, release_receiver) = bounded(1);
    if let Some(recovery) = &mut builder.recovery {
        recovery.verification_gate = Some((started_sender, release_receiver));
    }
    builder.handle_input(
        &fixture.bank,
        RewardInput::Certificates(vec![fixture.certificate(vote, &[0, 1, 2, 3])]),
    );
    let cluster_info = builder.cluster_info.clone();
    let my_pubkey = cluster_info.id();
    let banks = fixture.bank_forks.read().unwrap().sharable_banks();
    let exit = Arc::new(AtomicBool::new(false));
    let (input, input_receiver) = bounded(1);
    let (mut requestor, requests) = CertsRequestor::new();
    let service_exit = exit.clone();
    let service = thread::spawn(move || {
        run_with_builder_for_tests(
            service_exit,
            cluster_info,
            input_receiver,
            requests,
            banks,
            builder,
        )
    });
    let mut metrics = SlotMetrics::new(SLOT + NUM_SLOTS_FOR_REWARD, true);
    let request = requestor
        .request_reward_certs(&mut metrics, my_pubkey, SLOT + NUM_SLOTS_FOR_REWARD)
        .unwrap()
        .unwrap();
    // This gate is in recovery verification itself, rather than around request receipt.
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    // Wait only to let the service thread run. The production receive below still uses its
    // actual try_recv; the optional check remains blocked throughout this observation.
    let mut select = Select::new();
    select.recv(&request.reply_receiver);
    let ready = select.ready_timeout(Duration::from_secs(1)).is_ok();
    drop(select);
    let response = requestor
        .recv_reward_certs(&mut metrics, my_pubkey, Some(request))
        .unwrap();
    let _ = release.send(());
    drop(release);
    exit.store(true, Ordering::Relaxed);
    drop(input);
    drop(requestor);
    service.join().unwrap();
    assert!(ready, "recovery verification withheld the direct baseline");
    assert_eq!(metrics.reward_certs_skipped, 0);
    fixture.assert_response(&response, Some(&[0, 1]), None);
}
