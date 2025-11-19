//! Service in charge of ingesting new messages into the certificate pool
//! and notifying votor of new events that occur
use {
    crate::{
        commitment::{
            update_commitment_cache, CommitmentAggregationData, CommitmentError, CommitmentType,
        },
        consensus_pool::{
            parent_ready_tracker::BlockProductionParent, AddMessageError, ConsensusPool,
        },
        event::{LeaderWindowInfo, VotorEvent, VotorEventSender},
        voting_service::BLSOp,
        votor::Votor,
    },
    agave_votor_messages::consensus_message::{Certificate, ConsensusMessage},
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{
        blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache,
        leader_schedule_utils::last_of_consecutive_leader_slots,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    stats::Stats,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Condvar, Mutex,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

mod stats;

/// Inputs for the certificate pool thread
pub(crate) struct ConsensusPoolContext {
    pub(crate) exit: Arc<AtomicBool>,
    pub(crate) start: Arc<(Mutex<bool>, Condvar)>,

    pub(crate) cluster_info: Arc<ClusterInfo>,
    pub(crate) my_vote_pubkey: Pubkey,
    pub(crate) blockstore: Arc<Blockstore>,
    pub(crate) sharable_banks: SharableBanks,
    pub(crate) leader_schedule_cache: Arc<LeaderScheduleCache>,

    // TODO: for now we ingest our own votes into the certificate pool
    // just like regular votes. However do we need to convert
    // Vote -> BLSMessage -> Vote?
    // consider adding a separate pathway in consensus_pool.add_transaction for ingesting own votes
    pub(crate) consensus_message_receiver: Receiver<ConsensusMessage>,

    pub(crate) bls_sender: Sender<BLSOp>,
    pub(crate) event_sender: VotorEventSender,
    pub(crate) commitment_sender: Sender<CommitmentAggregationData>,

    pub(crate) delta_standstill: Duration,
}

pub(crate) struct ConsensusPoolService {
    t_ingest: JoinHandle<()>,
}

#[derive(Debug, Error)]
enum ServiceError {
    #[error("Failed to add message into the consensus pool: {0}")]
    AddMessage(#[from] AddMessageError),
    #[error("Channel {0} disconnected")]
    ChannelDisconnected(String),
    #[error("Channel is full")]
    ChannelFull,
    #[error("Failed to add block event: {0}")]
    FailedToAddBlockEvent(String),
}

impl ConsensusPoolService {
    pub(crate) fn new(mut ctx: ConsensusPoolContext) -> Self {
        let t_ingest = Builder::new()
            .name("solCnsPoolIngst".to_string())
            .spawn(move || {
                info!("ConsensusPoolService has started");
                if let Err(e) = Self::consensus_pool_ingest_loop(&mut ctx) {
                    ctx.exit.store(true, Ordering::Relaxed);
                    error!("ConsensusPoolService exited with error: {e}");
                }
                info!("ConsensusPoolService has stopped");
            })
            .unwrap();

        Self { t_ingest }
    }

    fn maybe_update_root_and_send_new_certificates(
        consensus_pool: &mut ConsensusPool,
        sharable_banks: &SharableBanks,
        bls_sender: &Sender<BLSOp>,
        new_finalized_slot: Option<Slot>,
        new_certificates_to_send: Vec<Arc<Certificate>>,
        standstill_timer: &mut Instant,
        stats: &mut Stats,
    ) -> Result<(), ServiceError> {
        // If we have a new finalized slot, update the root and send new certificates
        if new_finalized_slot.is_some() {
            // Reset standstill timer
            *standstill_timer = Instant::now();
            stats.new_finalized_slot += 1;
        }
        let root_bank = sharable_banks.root();
        consensus_pool.prune_old_state(root_bank.slot());
        stats.prune_old_state_called += 1;
        // Send new certificates to peers
        Self::send_certificates(bls_sender, new_certificates_to_send, stats)
    }

    fn send_certificates(
        bls_sender: &Sender<BLSOp>,
        certs: Vec<Arc<Certificate>>,
        stats: &mut Stats,
    ) -> Result<(), ServiceError> {
        let certs_len = certs.len();
        for (i, certificate) in certs.into_iter().enumerate() {
            // The BLS cert channel is expected to be large enough, so we don't
            // handle certificate re-send here.
            match bls_sender.try_send(BLSOp::PushCertificate { certificate }) {
                Ok(()) => {
                    stats.certificates_sent += 1;
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(ServiceError::ChannelDisconnected(
                        "VotingService".to_string(),
                    ));
                }
                Err(TrySendError::Full(_)) => {
                    stats.certificates_dropped += certs_len.saturating_sub(i);
                    return Err(ServiceError::ChannelFull);
                }
            }
        }
        Ok(())
    }

    fn process_consensus_message(
        ctx: &mut ConsensusPoolContext,
        my_pubkey: &Pubkey,
        message: ConsensusMessage,
        consensus_pool: &mut ConsensusPool,
        events: &mut Vec<VotorEvent>,
        standstill_timer: &mut Instant,
        stats: &mut Stats,
    ) -> Result<(), ServiceError> {
        match message {
            ConsensusMessage::Certificate(_) => {
                stats.received_certificates += 1;
            }
            ConsensusMessage::Vote(_) => {
                stats.received_votes += 1;
            }
        }
        let root_bank = ctx.sharable_banks.root();
        let (new_finalized_slot, new_certificates_to_send) =
            Self::add_message_and_maybe_update_commitment(
                &root_bank,
                my_pubkey,
                &ctx.my_vote_pubkey,
                message,
                consensus_pool,
                events,
                &ctx.commitment_sender,
            )?;
        Self::maybe_update_root_and_send_new_certificates(
            consensus_pool,
            &ctx.sharable_banks,
            &ctx.bls_sender,
            new_finalized_slot,
            new_certificates_to_send,
            standstill_timer,
            stats,
        )
    }

    // Main loop for the consensus pool service. Only exits when signalled or if
    // any channel is disconnected.
    fn consensus_pool_ingest_loop(ctx: &mut ConsensusPoolContext) -> Result<(), ServiceError> {
        let mut events = vec![];
        let mut my_pubkey = ctx.cluster_info.id();
        let root_bank = ctx.sharable_banks.root();
        let mut consensus_pool =
            ConsensusPool::new_from_root_bank(ctx.cluster_info.clone(), &root_bank);

        // Wait until migration has completed
        info!("{my_pubkey}: Consensus pool loop initialized, waiting for Alpenglow migration");
        Votor::wait_for_migration_or_exit(&ctx.exit, &ctx.start);
        info!("{my_pubkey}: Consensus pool loop starting");

        let mut stats = Stats::default();

        // Standstill tracking
        let mut standstill_timer = Instant::now();

        // Kick off parent ready
        let root_block = (root_bank.slot(), root_bank.block_id().unwrap_or_default());
        let mut highest_parent_ready = root_bank.slot();
        events.push(VotorEvent::ParentReady {
            slot: root_bank.slot().checked_add(1).unwrap(),
            parent_block: root_block,
        });

        // Ingest votes into consensus pool and notify voting loop of new events
        while !ctx.exit.load(Ordering::Relaxed) {
            // Update the current pubkey if it has changed
            let new_pubkey = ctx.cluster_info.id();
            if my_pubkey != new_pubkey {
                my_pubkey = new_pubkey;
                info!("Consensus pool pubkey updated to {my_pubkey}");
            }

            Self::add_produce_block_event(
                &mut highest_parent_ready,
                &consensus_pool,
                &my_pubkey,
                ctx,
                &mut events,
                &mut stats,
            )?;

            if standstill_timer.elapsed() > ctx.delta_standstill {
                events.push(VotorEvent::Standstill(
                    consensus_pool.highest_finalized_slot(),
                ));
                stats.standstill = true;
                standstill_timer = Instant::now();
                match Self::send_certificates(
                    &ctx.bls_sender,
                    consensus_pool.get_certs_for_standstill(),
                    &mut stats,
                ) {
                    Ok(()) => (),
                    Err(ServiceError::ChannelDisconnected(channel_name)) => {
                        return Err(ServiceError::ChannelDisconnected(channel_name));
                    }
                    Err(e) => {
                        trace!("{my_pubkey}: unable to push standstill certificates into pool {e}");
                    }
                }
            }

            events
                .drain(..)
                .try_for_each(|event| ctx.event_sender.send(event))
                .map_err(|_| {
                    ServiceError::ChannelDisconnected("Votor event receiver".to_string())
                })?;

            let consensus_message_receiver = ctx.consensus_message_receiver.clone();
            let messages = match consensus_message_receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(first_message) => {
                    std::iter::once(first_message).chain(consensus_message_receiver.try_iter())
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ServiceError::ChannelDisconnected(
                        "BLS receiver".to_string(),
                    ));
                }
            };

            for message in messages {
                match Self::process_consensus_message(
                    ctx,
                    &my_pubkey,
                    message,
                    &mut consensus_pool,
                    &mut events,
                    &mut standstill_timer,
                    &mut stats,
                ) {
                    Ok(()) => {}
                    Err(ServiceError::ChannelDisconnected(n)) => {
                        return Err(ServiceError::ChannelDisconnected(n));
                    }
                    Err(e) => {
                        warn!("{my_pubkey}: process_consensus_message() failed with {e}");
                        stats.add_message_failed += 1;
                    }
                }
            }
            stats.maybe_report();
            consensus_pool.maybe_report();
        }
        Ok(())
    }

    /// Adds a message to the consensus pool and updates the commitment cache if necessary
    ///
    /// Returns any newly finalized slot as well as any new certificates to broadcast out.
    /// Returns error if consensus message could not be added to the pool.
    fn add_message_and_maybe_update_commitment(
        root_bank: &Bank,
        my_pubkey: &Pubkey,
        my_vote_pubkey: &Pubkey,
        message: ConsensusMessage,
        consensus_pool: &mut ConsensusPool,
        votor_events: &mut Vec<VotorEvent>,
        commitment_sender: &Sender<CommitmentAggregationData>,
    ) -> Result<(Option<Slot>, Vec<Arc<Certificate>>), ServiceError> {
        let (new_finalized_slot, new_certificates_to_send) = consensus_pool.add_message(
            root_bank.epoch_schedule(),
            root_bank.epoch_stakes_map(),
            root_bank.slot(),
            my_vote_pubkey,
            message,
            votor_events,
        )?;
        let Some(new_finalized_slot) = new_finalized_slot else {
            return Ok((None, new_certificates_to_send));
        };
        trace!("{my_pubkey}: new finalization certificate for {new_finalized_slot}");
        update_commitment_cache(
            CommitmentType::Finalized,
            new_finalized_slot,
            commitment_sender,
        )
        .map_err(|e| match e {
            CommitmentError::ChannelDisconnected => {
                ServiceError::ChannelDisconnected("CommitmentSender".to_string())
            }
        })?;
        Ok((Some(new_finalized_slot), new_certificates_to_send))
    }

    fn add_produce_block_event(
        highest_parent_ready: &mut Slot,
        consensus_pool: &ConsensusPool,
        my_pubkey: &Pubkey,
        ctx: &mut ConsensusPoolContext,
        events: &mut Vec<VotorEvent>,
        stats: &mut Stats,
    ) -> Result<(), ServiceError> {
        let Some(new_highest_parent_ready) = events
            .iter()
            .filter_map(|event| match event {
                VotorEvent::ParentReady { slot, .. } => Some(slot),
                _ => None,
            })
            .max()
            .copied()
        else {
            return Ok(());
        };

        if new_highest_parent_ready <= *highest_parent_ready {
            return Ok(());
        }
        *highest_parent_ready = new_highest_parent_ready;

        let root_bank = ctx.sharable_banks.root();
        let Some(leader_pubkey) = ctx
            .leader_schedule_cache
            .slot_leader_at(*highest_parent_ready, Some(&root_bank))
        else {
            return Err(ServiceError::FailedToAddBlockEvent(format!(
                "Unable to compute the leader at slot {highest_parent_ready}. Something is wrong, \
                 exiting"
            )));
        };

        if &leader_pubkey != my_pubkey {
            return Ok(());
        }

        let start_slot = *highest_parent_ready;
        let end_slot = last_of_consecutive_leader_slots(start_slot);

        if (start_slot..=end_slot).any(|s| ctx.blockstore.has_existing_shreds_for_slot(s)) {
            warn!(
                "{my_pubkey}: We have already produced shreds in the window \
                 {start_slot}-{end_slot}, skipping production of our leader window"
            );
            return Ok(());
        }

        match consensus_pool
            .parent_ready_tracker
            .block_production_parent(start_slot)
        {
            BlockProductionParent::MissedWindow => {
                warn!(
                    "{my_pubkey}: Leader slot {start_slot} has already been certified, skipping \
                     production of {start_slot}-{end_slot}"
                );
                stats.parent_ready_missed_window += 1;
            }
            BlockProductionParent::ParentNotReady => {
                // This can't happen, place holder depending on how we hook up optimistic
                return Err(ServiceError::FailedToAddBlockEvent(
                    "Must have a block production parent".to_string(),
                ));
            }
            BlockProductionParent::Parent(parent_block) => {
                events.push(VotorEvent::ProduceWindow(LeaderWindowInfo {
                    start_slot,
                    end_slot,
                    parent_block,
                    skip_timer: Instant::now(),
                }));
                stats.parent_ready_produce_window += 1;
            }
        }

        Ok(())
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.t_ingest.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::common::DELTA_STANDSTILL,
        agave_votor_messages::{
            consensus_message::{CertificateType, VoteMessage, BLS_KEYPAIR_DERIVE_SEED},
            vote::Vote,
        },
        crossbeam_channel::Sender,
        solana_bls_signatures::{
            keypair::Keypair as BLSKeypair, signature::Signature as BLSSignature,
        },
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
        solana_hash::Hash,
        solana_ledger::get_tmp_ledger_path_auto_delete,
        solana_net_utils::SocketAddrSpace,
        solana_runtime::{
            bank_forks::{BankForks, SharableBanks},
            genesis_utils::{
                create_genesis_config_with_alpenglow_vote_accounts, ValidatorVoteKeypairs,
            },
        },
        solana_signer::Signer,
        std::sync::{Arc, Mutex},
        test_case::test_case,
    };

    struct ConsensusPoolServiceTestComponents {
        consensus_pool_service: ConsensusPoolService,
        consensus_message_sender: Sender<ConsensusMessage>,
        bls_receiver: Receiver<BLSOp>,
        event_receiver: Receiver<VotorEvent>,
        commitment_receiver: Receiver<CommitmentAggregationData>,
        validator_keypairs: Vec<ValidatorVoteKeypairs>,
        exit: Arc<AtomicBool>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        sharable_banks: SharableBanks,
    }

    fn setup(delta_standstill: Option<Duration>) -> ConsensusPoolServiceTestComponents {
        let (consensus_message_sender, consensus_message_receiver) = crossbeam_channel::unbounded();
        let (bls_sender, bls_receiver) = crossbeam_channel::unbounded();
        let (event_sender, event_receiver) = crossbeam_channel::unbounded();
        let (commitment_sender, commitment_receiver) = crossbeam_channel::unbounded();
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        // Make stake monotonic decreasing so rank is deterministic
        let stake = (0..validator_keypairs.len())
            .rev()
            .map(|i| (i.saturating_add(5).saturating_mul(100)) as u64)
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            stake,
        );
        let my_keypair = validator_keypairs[0].node_keypair.insecure_clone();
        let contact_info = ContactInfo::new_localhost(&my_keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new(
            contact_info,
            Arc::new(my_keypair),
            SocketAddrSpace::Unspecified,
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank0);

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let exit = Arc::new(AtomicBool::new(false));
        let leader_schedule_cache =
            Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));
        let ctx = ConsensusPoolContext {
            exit: exit.clone(),
            start: Arc::new((Mutex::new(true), Condvar::new())),
            cluster_info: Arc::new(cluster_info),
            my_vote_pubkey: Pubkey::new_unique(),
            blockstore: Arc::new(blockstore),
            sharable_banks: sharable_banks.clone(),
            leader_schedule_cache: leader_schedule_cache.clone(),
            consensus_message_receiver,
            bls_sender,
            event_sender,
            commitment_sender,
            delta_standstill: delta_standstill.unwrap_or(DELTA_STANDSTILL),
        };
        ConsensusPoolServiceTestComponents {
            consensus_pool_service: ConsensusPoolService::new(ctx),
            consensus_message_sender,
            bls_receiver,
            event_receiver,
            commitment_receiver,
            validator_keypairs,
            exit,
            leader_schedule_cache,
            sharable_banks,
        }
    }

    fn wait_for_event<T>(receiver: &Receiver<T>, condition: impl Fn(&T) -> bool) {
        let start = Instant::now();
        let mut event_received = false;
        while start.elapsed() < Duration::from_secs(5) {
            let res = receiver.recv_timeout(Duration::from_millis(500));
            if let Ok(event) = res {
                if condition(&event) {
                    event_received = true;
                    break;
                }
            }
        }
        assert!(event_received);
    }

    fn test_send_and_receive<T>(
        consensus_message_sender: &Sender<ConsensusMessage>,
        messages_to_send: Vec<ConsensusMessage>,
        receiver: &Receiver<T>,
        condition: impl Fn(&T) -> bool,
    ) {
        for message in messages_to_send {
            consensus_message_sender.send(message).unwrap();
        }
        wait_for_event(receiver, condition);
    }

    #[test]
    fn test_receive_and_send_consensus_message() {
        agave_logger::setup();
        let setup_result = setup(None);

        // validator 0 to 7 send Notarize on slot 2
        let block_id = Hash::new_unique();
        let target_slot = 2;
        let notarize_vote = Vote::new_notarization_vote(target_slot, block_id);
        let messages_to_send = (0..8)
            .map(|my_rank| {
                let vote_keypair = &setup_result.validator_keypairs[my_rank].vote_keypair;
                let bls_keypair =
                    BLSKeypair::derive_from_signer(vote_keypair, BLS_KEYPAIR_DERIVE_SEED).unwrap();
                let vote_serialized = bincode::serialize(&notarize_vote).unwrap();
                ConsensusMessage::Vote(VoteMessage {
                    vote: notarize_vote,
                    signature: bls_keypair.sign(&vote_serialized).into(),
                    rank: my_rank as u16,
                })
            })
            .collect();
        test_send_and_receive(
            &setup_result.consensus_message_sender,
            messages_to_send,
            &setup_result.bls_receiver,
            |event| {
                if let BLSOp::PushCertificate { certificate } = event {
                    assert_eq!(certificate.cert_type.slot(), target_slot);
                    let certificate_type = certificate.cert_type;
                    assert!(matches!(
                        certificate_type,
                        CertificateType::Notarize(_, _)
                            | CertificateType::FinalizeFast(_, _)
                            | CertificateType::NotarizeFallback(_, _)
                    ));
                    true
                } else {
                    false
                }
            },
        );
        // Verify that we received a finalized slot event
        wait_for_event(&setup_result.event_receiver, |event| {
            if let VotorEvent::Finalized((slot, receivied_block_id), is_fast_finalized) = event {
                assert_eq!(*slot, target_slot);
                assert_eq!(*receivied_block_id, block_id);
                assert!(*is_fast_finalized);
                true
            } else {
                false
            }
        });
        // Verify that we received a commitment update
        wait_for_event(
            &setup_result.commitment_receiver,
            |CommitmentAggregationData {
                 commitment_type,
                 slot,
                 ..
             }| {
                assert_eq!(*commitment_type, CommitmentType::Finalized);
                assert_eq!(*slot, target_slot);
                true
            },
        );

        // Now send a Skip certificate on slot 3, should be forwarded immediately
        let target_slot = 3;
        let skip_certificate = Certificate {
            cert_type: CertificateType::Skip(target_slot),
            signature: BLSSignature::default(),
            bitmap: vec![],
        };
        test_send_and_receive(
            &setup_result.consensus_message_sender,
            vec![ConsensusMessage::Certificate(skip_certificate)],
            &setup_result.bls_receiver,
            |event| {
                if let BLSOp::PushCertificate { certificate } = event {
                    matches!(certificate.cert_type, CertificateType::Skip(slot) if slot == target_slot)
                } else {
                    false
                }
            },
        );
        setup_result.exit.store(true, Ordering::Relaxed);
        setup_result.consensus_pool_service.join().unwrap();
    }

    #[test]
    fn test_send_produce_block_event() {
        let setup_result = setup(None);
        // Find when is the next leader slot for me (validator 0)
        let my_pubkey = setup_result.validator_keypairs[0].node_keypair.pubkey();
        let next_leader_slot = setup_result
            .leader_schedule_cache
            .next_leader_slot(
                &my_pubkey,
                0,
                &setup_result.sharable_banks.root(),
                None,
                1000000,
            )
            .expect("Should find a leader slot");
        // Send skip certificates for all slots up to the next leader slot
        let messages_to_send = (1..next_leader_slot.0)
            .map(|slot| {
                let skip_certificate = Certificate {
                    cert_type: CertificateType::Skip(slot),
                    signature: BLSSignature::default(),
                    bitmap: vec![],
                };
                ConsensusMessage::Certificate(skip_certificate)
            })
            .collect();
        test_send_and_receive(
            &setup_result.consensus_message_sender,
            messages_to_send,
            &setup_result.event_receiver,
            |event| {
                if let VotorEvent::ProduceWindow(LeaderWindowInfo { start_slot, .. }) = event {
                    assert_eq!(*start_slot, next_leader_slot.0);
                    true
                } else {
                    false
                }
            },
        );
        setup_result.exit.store(true, Ordering::Relaxed);
        setup_result.consensus_pool_service.join().unwrap();
    }

    #[test]
    fn test_send_standstill() {
        let delta_standstill_for_test = Duration::from_millis(100);
        let setup_result = setup(Some(delta_standstill_for_test));
        thread::sleep(delta_standstill_for_test);
        // Verify that we received a standstill event
        wait_for_event(
            &setup_result.event_receiver,
            |event| matches!(event, VotorEvent::Standstill(slot) if *slot == 0),
        );
        setup_result.exit.store(true, Ordering::Relaxed);
        setup_result.consensus_pool_service.join().unwrap();
    }

    #[test_case("consensus_message_receiver")]
    #[test_case("bls_receiver")]
    #[test_case("votor_event_receiver")]
    #[test_case("commitment_receiver")]
    fn test_channel_disconnection(channel_name: &str) {
        let setup_result = setup(None);
        // A lot of the receiver needs a finalize certificate to trigger an exit
        if channel_name != "consensus_message_receiver" {
            let finalize_certificate = Certificate {
                cert_type: CertificateType::FinalizeFast(2, Hash::new_unique()),
                signature: BLSSignature::default(),
                bitmap: vec![],
            };
            setup_result
                .consensus_message_sender
                .send(ConsensusMessage::Certificate(finalize_certificate))
                .unwrap();
        }
        match channel_name {
            "consensus_message_receiver" => {
                drop(setup_result.consensus_message_sender);
            }
            "bls_receiver" => {
                drop(setup_result.bls_receiver);
            }
            "votor_event_receiver" => {
                drop(setup_result.event_receiver);
            }
            "commitment_receiver" => {
                drop(setup_result.commitment_receiver);
            }
            _ => panic!("Unknown channel name"),
        }
        // Verify that the consensus pool service exits within 5 seconds
        let start = Instant::now();
        while !setup_result.exit.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(100));
        }
        assert!(setup_result.exit.load(Ordering::Relaxed));
        setup_result.consensus_pool_service.join().unwrap();
    }
}
