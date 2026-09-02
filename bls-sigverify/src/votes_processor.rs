use {
    crate::{
        VerifiedVote,
        errors::SigVerifyVoteError,
        rewards::{RewardInput, rewards_wants_vote},
        stats::VoteProcessorStats,
        utils::{
            send_sig_verified_batch_to_pool, send_votes_to_metrics, send_votes_to_repair,
            send_votes_to_rewards,
        },
    },
    agave_votor_messages::{
        VerifiedVotorSlotsMessage,
        metric_types::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
        vote::Vote,
    },
    crossbeam_channel::{Receiver, Sender, select},
    log::error,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    solana_streamer::evicting_sender::EvictingSender,
    std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    },
};

#[derive(Default)]
pub(crate) struct ProcessedVotes {
    reward_msg: Vec<VoteAggregate>,
    repair_msg: HashMap<Slot, Vec<Pubkey>>,
    vote_aggregates_for_pool: Vec<VoteAggregate>,
    metrics_msg: Vec<ConsensusMetricsEvent>,
}

pub(crate) struct VotesProcessor {
    exit: Arc<AtomicBool>,
    sharable_banks: SharableBanks,
    cluster_info: Arc<ClusterInfo>,
    leader_schedule: Arc<LeaderScheduleCache>,
    channel_to_repair: EvictingSender<VerifiedVotorSlotsMessage>,
    channel_to_reward: Sender<RewardInput>,
    channel_to_pool: Sender<SigVerifiedBatch>,
    channel_to_metrics: ConsensusMetricsEventSender,
    verified_votes_receiver: Receiver<Vec<Vec<VerifiedVote>>>,
    stats: VoteProcessorStats,
}

impl VotesProcessor {
    pub(crate) fn new(
        exit: Arc<AtomicBool>,
        verified_votes_receiver: Receiver<Vec<Vec<VerifiedVote>>>,
        sharable_banks: SharableBanks,
        cluster_info: Arc<ClusterInfo>,
        leader_schedule: Arc<LeaderScheduleCache>,
        channel_to_repair: EvictingSender<VerifiedVotorSlotsMessage>,
        channel_to_reward: Sender<RewardInput>,
        channel_to_pool: Sender<SigVerifiedBatch>,
        channel_to_metrics: ConsensusMetricsEventSender,
    ) -> Self {
        Self {
            exit,
            verified_votes_receiver,
            sharable_banks,
            cluster_info,
            leader_schedule,
            channel_to_repair,
            channel_to_reward,
            channel_to_pool,
            channel_to_metrics,
            stats: VoteProcessorStats::default(),
        }
    }

    fn recv(&self) -> Result<Vec<Vec<VerifiedVote>>, ()> {
        while !self.exit.load(Ordering::Relaxed) {
            select! {
                recv(self.verified_votes_receiver) -> msg => {
                    return msg.map_err(|_| ())
                }
                default(Duration::from_secs(1)) => continue,
            }
        }
        Err(())
    }

    fn send_msgs(
        &mut self,
        processed_votess: Vec<ProcessedVotes>,
    ) -> Result<(), SigVerifyVoteError> {
        let my_pubkey = &self.cluster_info.id();
        for processed_votes in processed_votess {
            send_sig_verified_batch_to_pool(
                my_pubkey,
                processed_votes.vote_aggregates_for_pool,
                &self.channel_to_pool,
                &mut self.stats,
            )?;
            send_votes_to_repair(
                my_pubkey,
                processed_votes.repair_msg,
                &self.channel_to_repair,
                &mut self.stats,
            );
            send_votes_to_rewards(
                my_pubkey,
                processed_votes.reward_msg,
                &self.channel_to_reward,
                &mut self.stats,
            );
            send_votes_to_metrics(
                my_pubkey,
                processed_votes.metrics_msg,
                &self.channel_to_metrics,
                &mut self.stats,
            );
        }
        Ok(())
    }

    pub(crate) fn run(mut self) {
        while !self.exit.load(Ordering::Relaxed) {
            let Ok(verified_votes) = self.recv() else {
                error!("verified votes receiver channel disconnected.  Exiting.");
                break;
            };
            let root_bank = self.sharable_banks.root();
            let processed_votes = process_verified_votes(
                verified_votes,
                &root_bank,
                &self.cluster_info,
                &self.leader_schedule,
            );
            if let Err(e) = self.send_msgs(processed_votes) {
                error!("sending msgs failed with {e:?}.  Exiting.");
                break;
            }
            self.stats.iterations += 1;
            self.stats.maybe_report();
        }
    }
}

/// Processes the verified votes for various downstream services.
///
/// In particular, collects and returns the relevant messages for the consensus pool; rewards;
/// repair; and metrics;
fn process_verified_votes(
    verified_votess: Vec<Vec<VerifiedVote>>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
) -> Vec<ProcessedVotes> {
    let mut ret = vec![];
    for verified_votes in verified_votess {
        let mut votes_for_reward = Vec::with_capacity(verified_votes.len());
        let mut msgs_for_repair = HashMap::new();
        let mut vote_aggregates_for_pool = Vec::with_capacity(verified_votes.len());
        let mut votes_for_metrics = Vec::with_capacity(verified_votes.len());
        for payload in verified_votes {
            inspect_for_repair(&payload, &mut msgs_for_repair);

            for pubkey in &payload.sender_vote_account_pubkeys {
                votes_for_metrics.push(ConsensusMetricsEvent::Vote {
                    id: *pubkey,
                    vote: *payload.vote_aggregate.vote(),
                });
            }
            if rewards_wants_vote(
                cluster_info,
                leader_schedule,
                root_bank.slot(),
                payload.vote_aggregate.vote(),
            ) {
                votes_for_reward.push(payload.vote_aggregate.clone());
            }
            vote_aggregates_for_pool.push(payload.vote_aggregate);
        }
        let p = ProcessedVotes {
            reward_msg: votes_for_reward,
            repair_msg: msgs_for_repair,
            vote_aggregates_for_pool,
            metrics_msg: votes_for_metrics,
        };
        ret.push(p);
    }
    ret
}

/// If the vote is relevant to repair, then adds it to the [`msgs_for_repair`] so it can eventually
/// be sent to repair.
fn inspect_for_repair(vote: &VerifiedVote, msgs_for_repair: &mut HashMap<Slot, Vec<Pubkey>>) {
    let vote_slot = vote.vote_aggregate.vote().slot();
    match vote.vote_aggregate.vote() {
        Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
            msgs_for_repair
                .entry(vote_slot)
                .or_default()
                .extend(&vote.sender_vote_account_pubkeys);
        }
        Vote::Skip(_) | Vote::SkipFallback(_) | Vote::Genesis(_) => (),
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor_messages::{
            consensus_message::Block,
            reward_certificate::NUM_SLOTS_FOR_REWARD,
            wire::{VotePayloadToSign, get_vote_payload_to_sign},
        },
        crossbeam_channel::unbounded,
        rayon::prelude::*,
        solana_bls_signatures::{Keypair as BlsKeypair, Signature, SignatureProjective},
        solana_epoch_schedule::EpochSchedule,
        solana_gossip::contact_info::ContactInfo,
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_runtime::{
            bank::SlotLeader,
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
        std::num::NonZero,
    };

    struct TestContext {
        processor: VotesProcessor,
        pool: Receiver<SigVerifiedBatch>,
        rewards: Receiver<RewardInput>,
        metrics: Receiver<(std::time::Instant, Vec<ConsensusMetricsEvent>)>,
        _repair: Receiver<HashMap<Slot, Vec<Pubkey>>>,
        _bank_forks: Arc<std::sync::RwLock<BankForks>>,
    }

    impl TestContext {
        fn new(root_slot: Slot, is_leader: bool) -> Self {
            let validator = ValidatorVoteKeypairs::new_rand();
            let mut genesis = create_genesis_config_with_alpenglow_vote_accounts(
                1_000_000_000,
                std::slice::from_ref(&validator),
                vec![1_000],
            );
            genesis.genesis_config.epoch_schedule = EpochSchedule::without_warmup();
            let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
            let sharable_banks = bank_forks.read().unwrap().sharable_banks();
            if root_slot > 0 {
                let bank =
                    Bank::new_from_parent(sharable_banks.root(), SlotLeader::default(), root_slot);
                let mut forks = bank_forks.write().unwrap();
                forks.insert(bank);
                forks.set_root(root_slot, None, None);
            }
            // With one validator, every scheduled slot belongs to this identity.
            let keypair = if is_leader {
                validator.node_keypair
            } else {
                Keypair::new()
            };
            let cluster_info = Arc::new(ClusterInfo::new(
                ContactInfo::new_localhost(&keypair.pubkey(), 0),
                Arc::new(keypair),
                SocketAddrSpace::Unspecified,
            ));
            let leader_schedule =
                Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));
            let (_, votes_receiver) = unbounded();
            let (pool_sender, pool) = unbounded();
            let (reward_sender, rewards) = unbounded();
            let (metrics_sender, metrics) = unbounded();
            let (repair_sender, repair) = EvictingSender::new_bounded(1024);
            let processor = VotesProcessor::new(
                Arc::new(AtomicBool::new(false)),
                votes_receiver,
                sharable_banks,
                cluster_info,
                leader_schedule,
                repair_sender,
                reward_sender,
                pool_sender,
                metrics_sender,
            );
            Self {
                processor,
                pool,
                rewards,
                metrics,
                _repair: repair,
                _bank_forks: bank_forks,
            }
        }

        fn process(&mut self, votes: Vec<VerifiedVote>) {
            let processed = process_verified_votes(
                vec![votes],
                &self.processor.sharable_banks.root(),
                &self.processor.cluster_info,
                &self.processor.leader_schedule,
            );
            self.processor.send_msgs(processed).unwrap();
        }

        fn received_rewards(&self) -> Vec<VoteAggregate> {
            self.rewards
                .try_iter()
                .flat_map(|input| match input {
                    RewardInput::External(votes) => votes,
                    RewardInput::Own(_) => panic!("expected external votes"),
                })
                .collect()
        }
    }

    fn verified_vote(vote: Vote, voters: &[Pubkey]) -> VerifiedVote {
        let payload = get_vote_payload_to_sign(vote, 0);
        let signatures = voters
            .iter()
            .map(|_| Signature::from(BlsKeypair::new().sign(&payload)))
            .collect::<Vec<_>>();
        let signature = SignatureProjective::par_aggregate(signatures.par_iter()).unwrap();
        VerifiedVote {
            vote_aggregate: VoteAggregate::new_from_verified_votes(
                voters.len(),
                VotePayloadToSign::new_from_vote(vote, 0),
                (0..voters.len()).map(|rank| {
                    (
                        rank as u16,
                        NonZero::new((rank.saturating_add(100)) as u64).unwrap(),
                    )
                }),
                signature,
            ),
            sender_vote_account_pubkeys: voters.to_vec(),
        }
    }

    #[test]
    fn rewards_include_only_notar_and_skip_and_preserve_aggregates() {
        let mut ctx = TestContext::new(0, true);
        let voters = [Pubkey::new_unique(), Pubkey::new_unique()];
        let block = Block::new_unique(5);
        let votes = [
            Vote::new_notarization_vote(block),
            Vote::new_skip_vote(5),
            Vote::new_finalization_vote(5),
            Vote::new_notarization_fallback_vote(block),
            Vote::new_skip_fallback_vote(5),
            Vote::new_genesis_vote(block),
        ]
        .map(|vote| verified_vote(vote, &voters));
        let expected = votes
            .iter()
            .map(|v| v.vote_aggregate.clone())
            .collect::<Vec<_>>();
        ctx.process(Vec::from(votes));
        // Full equality checks ranks, stake, signature, and vote are preserved.
        assert_eq!(ctx.received_rewards(), expected[..2]);
        assert_eq!(
            ctx.pool.try_recv().unwrap(),
            SigVerifiedBatch::Votes(expected)
        );
    }

    #[test]
    fn reward_window_boundary_uses_root_slot() {
        let mut ctx = TestContext::new(5 + NUM_SLOTS_FOR_REWARD, true);
        let voters = [Pubkey::new_unique()];
        let mut votes = Vec::new();
        let mut expected = Vec::new();
        for slot in [4, 5, 6] {
            for vote in [Vote::new_skip_vote(slot), Vote::new_unique_notar(slot)] {
                let verified = verified_vote(vote, &voters);
                if slot == 6 {
                    expected.push(verified.vote_aggregate.clone());
                }
                votes.push(verified);
            }
        }
        ctx.process(votes);
        assert_eq!(ctx.received_rewards(), expected);
    }

    #[test]
    fn rewards_require_local_leadership_and_known_schedule() {
        for is_leader in [false, true] {
            let mut ctx = TestContext::new(0, is_leader);
            if is_leader {
                ctx.processor
                    .leader_schedule
                    .cached_schedules
                    .write()
                    .unwrap()
                    .0
                    .clear();
            }
            let voters = [Pubkey::new_unique()];
            ctx.process(vec![
                verified_vote(Vote::new_skip_vote(5), &voters),
                verified_vote(Vote::new_unique_notar(6), &voters),
            ]);
            assert!(ctx.received_rewards().is_empty());
            let SigVerifiedBatch::Votes(votes) = ctx.pool.try_recv().unwrap() else {
                panic!("expected votes");
            };
            assert_eq!(votes.len(), 2);
        }
    }

    #[test]
    fn metrics_report_each_vote_account_in_each_aggregate() {
        let mut ctx = TestContext::new(0, true);
        let voters = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let notar = Vote::new_unique_notar(5);
        let finalize = Vote::new_finalization_vote(5);
        ctx.process(vec![
            verified_vote(notar, &voters),
            verified_vote(finalize, &voters[..2]),
        ]);
        let events = ctx
            .metrics
            .try_iter()
            .flat_map(|(_, events)| events)
            .collect::<Vec<_>>();
        let expected = voters
            .iter()
            .map(|id| ConsensusMetricsEvent::Vote {
                id: *id,
                vote: notar,
            })
            .chain(voters[..2].iter().map(|id| ConsensusMetricsEvent::Vote {
                id: *id,
                vote: finalize,
            }))
            .collect::<Vec<_>>();
        assert_eq!(events, expected);
    }
}
