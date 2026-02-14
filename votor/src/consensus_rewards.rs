use {
    agave_votor_messages::{
        consensus_message::VoteMessage,
        reward_certificate::{
            AddVoteMessage, BuildRewardCertsRequest, BuildRewardCertsRespError,
            BuildRewardCertsRespSucc, BuildRewardCertsResponse, NUM_SLOTS_FOR_REWARD,
        },
        vote::Vote,
    },
    crossbeam_channel::{select_biased, Receiver, Sender},
    entry::Entry,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    std::{
        collections::BTreeMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

mod entry;

/// Returns [`false`] if the rewards container is not interested in the [`VoteMessage`].
/// Returns [`true`] if the rewards container might be interested in the [`VoteMessage`].
pub fn wants_vote(
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    root_slot: Slot,
    vote: &VoteMessage,
) -> bool {
    match vote.vote {
        Vote::Notarize(_) | Vote::Skip(_) => (),
        Vote::Finalize(_)
        | Vote::NotarizeFallback(_)
        | Vote::SkipFallback(_)
        | Vote::Genesis(_) => return false,
    }
    let vote_slot = vote.vote.slot();
    if vote_slot.saturating_add(NUM_SLOTS_FOR_REWARD) <= root_slot {
        return false;
    }
    let my_pubkey = cluster_info.id();
    let Some(leader) =
        leader_schedule.slot_leader_at(vote_slot.saturating_add(NUM_SLOTS_FOR_REWARD), None)
    else {
        return false;
    };
    if leader.id != my_pubkey {
        return false;
    }
    true
}

/// Container to store state needed to generate reward certificates.
struct ConsensusRewards {
    /// Per [`Slot`], stores skip and notar votes.
    votes: BTreeMap<Slot, Entry>,
    /// Stores the latest pubkey for the current node.
    cluster_info: Arc<ClusterInfo>,
    /// Stores the leader schedules.
    leader_schedule: Arc<LeaderScheduleCache>,
    sharable_banks: SharableBanks,
    /// Flag to indicate when the channel receiving loop should exit.
    exit: Arc<AtomicBool>,
    /// Channel to receive messages to build reward certificates.
    build_reward_certs_receiver: Receiver<BuildRewardCertsRequest>,
    /// Channel send the built reward certificates.
    reward_certs_sender: Sender<BuildRewardCertsResponse>,
    /// Channel to receive verified votes.
    votes_receiver: Receiver<AddVoteMessage>,
}

impl ConsensusRewards {
    /// Constructs a new instance of [`ConsensusRewards`].
    fn new(
        cluster_info: Arc<ClusterInfo>,
        leader_schedule: Arc<LeaderScheduleCache>,
        sharable_banks: SharableBanks,
        exit: Arc<AtomicBool>,
        build_reward_certs_receiver: Receiver<BuildRewardCertsRequest>,
        reward_certs_sender: Sender<BuildRewardCertsResponse>,
        votes_receiver: Receiver<AddVoteMessage>,
    ) -> Self {
        Self {
            votes: BTreeMap::default(),
            cluster_info,
            leader_schedule,
            sharable_banks,
            exit,
            build_reward_certs_receiver,
            reward_certs_sender,
            votes_receiver,
        }
    }

    /// Runs a loop receiving and handling messages over different channels.
    fn run(&mut self) {
        while !self.exit.load(Ordering::Relaxed) {
            // bias messages to build certificates as that is on the critical path
            select_biased! {
                recv(self.build_reward_certs_receiver) -> msg => {
                    match msg {
                        Ok(msg) => {
                            let resp = self.build_certs(msg.bank_slot);
                            if self.reward_certs_sender.send(resp).is_err() {
                                warn!("cert sender channel is disconnected; exiting.");
                                break;
                            }
                        }
                        Err(_) => {
                            warn!("build reward certs channel is disconnected; exiting.");
                            break;
                        }
                    }
                }
                recv(self.votes_receiver) -> msg => {
                    match msg {
                        Ok(msg) => {
                            let bank = self.sharable_banks.root();
                            for vote in msg.votes {
                                self.add_vote(&bank, &vote);
                            }
                        }
                        Err(_) => {
                            warn!("votes receiver channel is disconnected; exiting.");
                            break;
                        }
                    }
                }
                default(Duration::from_secs(1)) => {
                    continue;
                }
            }
        }
    }

    /// Returns [`true`] if the rewards container is interested in this vote else [`false`].
    fn wants_vote(&self, root_slot: Slot, vote: &VoteMessage) -> bool {
        if !wants_vote(&self.cluster_info, &self.leader_schedule, root_slot, vote) {
            return false;
        }
        let Some(entry) = self.votes.get(&vote.vote.slot()) else {
            return true;
        };
        entry.wants_vote(vote)
    }

    /// Adds received [`VoteMessage`] from other validators.
    fn add_vote(&mut self, root_bank: &Bank, vote: &VoteMessage) {
        let slot = vote.vote.slot();
        let Some(rank_map) = root_bank.get_rank_map(slot) else {
            warn!(
                "failed to look up rank_map for slot {slot} using bank for slot {}",
                root_bank.slot()
            );
            return;
        };
        let max_validators = rank_map.len();
        let root_slot = root_bank.slot();
        // drop state that is too old based on how the root slot has progressed
        self.votes = self.votes.split_off(
            &(root_slot
                .saturating_add(NUM_SLOTS_FOR_REWARD)
                .saturating_add(1)),
        );

        if !self.wants_vote(root_slot, vote) {
            return;
        }
        match self
            .votes
            .entry(vote.vote.slot())
            .or_insert(Entry::new(max_validators))
            .add_vote(rank_map, vote)
        {
            Ok(()) => (),
            Err(e) => {
                warn!("Adding vote {vote:?} failed with {e}");
            }
        }
    }

    /// Builds reward certificates.
    fn build_certs(
        &mut self,
        bank_slot: Slot,
    ) -> Result<BuildRewardCertsRespSucc, BuildRewardCertsRespError> {
        let Some(reward_slot) = bank_slot.checked_sub(NUM_SLOTS_FOR_REWARD) else {
            return Ok(BuildRewardCertsRespSucc::default());
        };
        // we assume that the block creation loop will only ever request to build reward certs in a strictly increasing order so we can drop older state
        self.votes = self.votes.split_off(&reward_slot);
        match self.votes.remove(&reward_slot) {
            None => Ok(BuildRewardCertsRespSucc::default()),
            Some(entry) => entry.build_certs(reward_slot),
        }
    }
}

/// Service to run the consensus reward container in a dedicated thread.
pub struct ConsensusRewardsService {
    handle: JoinHandle<()>,
}

impl ConsensusRewardsService {
    /// Creates a new instance of [`ConsensusRewardsService`].
    pub fn new(
        cluster_info: Arc<ClusterInfo>,
        leader_schedule: Arc<LeaderScheduleCache>,
        sharable_banks: SharableBanks,
        exit: Arc<AtomicBool>,
        votes_receiver: Receiver<AddVoteMessage>,
        build_reward_certs_receiver: Receiver<BuildRewardCertsRequest>,
        reward_certs_sender: Sender<BuildRewardCertsResponse>,
    ) -> Self {
        let handle = Builder::new()
            .name("solConsRew".to_string())
            .spawn(move || {
                ConsensusRewards::new(
                    cluster_info,
                    leader_schedule,
                    sharable_banks,
                    exit,
                    build_reward_certs_receiver,
                    reward_certs_sender,
                    votes_receiver,
                )
                .run();
            })
            .unwrap();
        Self { handle }
    }

    pub fn join(self) -> thread::Result<()> {
        self.handle.join()
    }
}
