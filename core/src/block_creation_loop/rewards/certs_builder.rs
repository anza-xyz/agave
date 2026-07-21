use {
    crate::block_creation_loop::rewards::msg_types::{
        RewardRequest, RewardRespSucc, RewardResponse,
    },
    agave_bls_sigverify::rewards::RewardVoteMessage,
    agave_votor_messages::reward_certificate::{BuildRewardCertsRespError, NUM_SLOTS_FOR_REWARD},
    crossbeam_channel::RecvError,
    entry::Entry,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_runtime::bank::Bank,
    std::{collections::BTreeMap, sync::Arc},
};

mod entry;

/// Container to store state needed to generate reward certificates.
pub(super) struct CertsBuilder {
    /// Per [`Slot`], stores skip and notar votes.
    votes: BTreeMap<Slot, Entry>,
    /// Stores the latest pubkey for the current node.
    cluster_info: Arc<ClusterInfo>,
}

impl CertsBuilder {
    /// Constructs a new instance of [`CertsBuilder`].
    pub(super) fn new(cluster_info: Arc<ClusterInfo>) -> Self {
        Self {
            votes: BTreeMap::default(),
            cluster_info,
        }
    }

    /// Builds reward certificates.
    fn build_certs(
        &mut self,
        bank_slot: Slot,
    ) -> Result<RewardRespSucc, BuildRewardCertsRespError> {
        let Some(reward_slot) = bank_slot.checked_sub(NUM_SLOTS_FOR_REWARD) else {
            return Ok(RewardRespSucc::default());
        };
        // we assume that the block creation loop will only ever request to build reward certs in a
        // strictly increasing order so we can drop older state
        self.votes = self.votes.split_off(&reward_slot);
        match self.votes.remove(&reward_slot) {
            None => Ok(RewardRespSucc::default()),
            Some(entry) => entry.build_certs(reward_slot),
        }
    }

    pub(super) fn build_request(
        &mut self,
        request: Result<RewardRequest, RecvError>,
    ) -> Result<(), ()> {
        let my_pubkey = self.cluster_info.id();
        match request {
            Ok(RewardRequest {
                bank_slot,
                reply_sender,
            }) => {
                let resp = RewardResponse {
                    result: self.build_certs(bank_slot),
                };
                let _ = reply_sender.send(resp).inspect_err(|_| {
                    info!(
                        "{my_pubkey}: channel to send reply for bank_slot={bank_slot} disconnected"
                    );
                });
                Ok(())
            }
            Err(_) => {
                error!("{my_pubkey}: build reward certs channel is disconnected; exiting.");
                Err(())
            }
        }
    }

    /// Returns [`true`] if the rewards container is interested in this vote else [`false`].
    fn wants_vote(&self, msg: &RewardVoteMessage) -> bool {
        let Some(entry) = self.votes.get(&msg.vote.slot()) else {
            return true;
        };
        entry.wants_vote(msg)
    }

    /// Adds received [`VoteMessage`] from other validators.
    pub(super) fn add_vote(&mut self, root_bank: &Bank, msg: RewardVoteMessage) {
        let slot = msg.vote.slot();
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
        // TODO: if this actually purges state, that probably indicates that the leader missed its
        // window.  We should have a metric for this.
        self.votes = self
            .votes
            .split_off(&root_slot.saturating_sub(NUM_SLOTS_FOR_REWARD));

        if !self.wants_vote(&msg) {
            return;
        }
        match self
            .votes
            .entry(msg.vote.slot())
            .or_insert(Entry::new(max_validators))
            .add_vote(&msg)
        {
            Ok(()) => (),
            Err(e) => {
                warn!("Adding vote {msg:?} failed with {e}");
            }
        }
    }
}
