use {
    crate::block_creation_loop::{SlotMetrics, rewards::msg_types::RewardRespSucc},
    agave_bls_sigverify::{rewards::RewardInput, sig_verified_messages::VoteAggregate},
    agave_votor::aggregate_accumulator::AggregateAccumulatorError,
    agave_votor_messages::{
        consensus_message::VoteMessage,
        reward_certificate::{BuildRewardCertsRespError, NUM_SLOTS_FOR_REWARD},
    },
    entry::Entry,
    solana_clock::Slot,
    solana_measure::measure_us,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::{
        collections::BTreeMap,
        sync::Mutex,
        time::{Duration, Instant},
    },
};

mod entry;

const REPORT_INTERVAL: Duration = Duration::from_secs(1);

struct Metrics {
    last_report: Instant,
    purged_state: u64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            purged_state: 0,
        }
    }
}

impl Metrics {
    fn maybe_report(&mut self) {
        if self.last_report.elapsed() > REPORT_INTERVAL {
            datapoint_info!(
                "reward-certs-builder",
                ("purged_state", self.purged_state, i64)
            );
            *self = Self::default();
        }
    }
}

/// Votes for building reward certificates.
///
/// Shared between the rewards service, which adds votes as they arrive, and the block creation
/// loop, which builds a block's reward certificates when it produces the block's footer.
#[derive(Default)]
pub(crate) struct CertsBuilder {
    /// Per [`Slot`], stores the skip and notar votes.
    aggregates: BTreeMap<Slot, Entry>,
    metrics: Metrics,
}

impl CertsBuilder {
    pub(super) fn maybe_report(&mut self) {
        self.metrics.maybe_report();
    }

    /// Drops the votes for slots before `slot`.
    fn purge_before(&mut self, slot: Slot) {
        let new_aggregates = self.aggregates.split_off(&slot);
        if !self.aggregates.is_empty() {
            self.metrics.purged_state += 1;
        }
        self.aggregates = new_aggregates;
    }

    fn add_aggregate(
        &mut self,
        vote_slot: Slot,
        max_validators: usize,
        aggregate: VoteAggregate,
        vote_account_pubkeys: Vec<Pubkey>,
    ) -> Result<(), AggregateAccumulatorError> {
        self.aggregates
            .entry(vote_slot)
            .or_insert_with(|| Entry::new(max_validators))
            .add_aggregate(aggregate, vote_account_pubkeys)
    }

    fn add_own_msg(
        &mut self,
        vote_slot: Slot,
        max_validators: usize,
        vote_msg: VoteMessage,
        vote_account_pubkey: Pubkey,
    ) -> Result<(), AggregateAccumulatorError> {
        self.aggregates
            .entry(vote_slot)
            .or_insert_with(|| Entry::new(max_validators))
            .add_own_msg(vote_msg, vote_account_pubkey)
    }

    /// Removes the votes for `reward_slot`, dropping those for older slots too.
    ///
    /// The block creation loop builds reward certs for strictly increasing slots, so older votes
    /// are no longer needed.
    fn take(&mut self, reward_slot: Slot) -> Option<Entry> {
        self.aggregates = self.aggregates.split_off(&reward_slot);
        self.aggregates.remove(&reward_slot)
    }
}

/// Builds the reward certificates for the footer of the block at `bank_slot`, from every vote added
/// so far.
///
/// The lock is held only to take the reward slot's votes; the certificates are built after it is
/// released.
pub(crate) fn build_reward_certs(
    builder: &Mutex<CertsBuilder>,
    bank_slot: Slot,
    slot_metrics: &mut SlotMetrics,
) -> Result<RewardRespSucc, BuildRewardCertsRespError> {
    let Some(reward_slot) = bank_slot.checked_sub(NUM_SLOTS_FOR_REWARD) else {
        return Ok(RewardRespSucc::default());
    };
    let (entry, take_us) = measure_us!(builder.lock().unwrap().take(reward_slot));
    slot_metrics.reward_certs_take_us = take_us;
    let Some(entry) = entry else {
        return Ok(RewardRespSucc::default());
    };
    let (result, build_us) = measure_us!(entry.build_certs(reward_slot));
    slot_metrics.reward_certs_build_us = build_us;
    result
}

/// Adds the votes in `input` to `builder`.
///
/// The lock is taken once per vote, so the block creation loop never waits behind a whole batch.
pub(super) fn handle_input(builder: &Mutex<CertsBuilder>, root_bank: &Bank, input: RewardInput) {
    let root_slot = root_bank.slot();
    // drop state that is too old based on how the root slot has progressed
    // if this actually drops state, that probably indicates that the leader missed its
    // window.
    builder
        .lock()
        .unwrap()
        .purge_before(root_slot.saturating_sub(NUM_SLOTS_FOR_REWARD));

    match input {
        RewardInput::External(aggregates) => {
            for aggregate in aggregates {
                let vote = *aggregate.vote();
                let vote_slot = vote.slot();
                let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                    warn!(
                        "failed to look up rank_map for slot {vote_slot} using bank for slot {}",
                        root_bank.slot()
                    );
                    return;
                };
                let max_validators = rank_map.len();
                let mut vote_account_pubkeys = vec![];
                for rank in aggregate.ranks().iter_ones() {
                    let Some(stake_entry) = rank_map.get_pubkey_stake_entry(rank) else {
                        return;
                    };
                    vote_account_pubkeys.push(stake_entry.vote_account_pubkey);
                }

                if let Err(e) = builder.lock().unwrap().add_aggregate(
                    vote_slot,
                    max_validators,
                    aggregate,
                    vote_account_pubkeys,
                ) {
                    warn!("Adding aggregate with vote {vote:?} failed with {e}");
                }
            }
        }
        RewardInput::Own(vote_msg) => {
            let vote = vote_msg.vote;
            let vote_slot = vote.slot();
            let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                warn!(
                    "failed to look up rank_map for slot {vote_slot} using bank for slot {}",
                    root_bank.slot()
                );
                return;
            };
            let max_validators = rank_map.len();
            let Some(stake_entry) = rank_map.get_pubkey_stake_entry(vote_msg.rank as usize) else {
                return;
            };

            if let Err(e) = builder.lock().unwrap().add_own_msg(
                vote_slot,
                max_validators,
                vote_msg,
                stake_entry.vote_account_pubkey,
            ) {
                warn!("Adding aggregate with vote {vote:?} failed with {e}");
            }
        }
    }
    builder.lock().unwrap().maybe_report();
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor_messages::{
            consensus_message::{Block, BlockId},
            vote::Vote,
        },
        entry::tests::{get_keypairs, new_reward_vote_aggregate},
    };

    #[test]
    fn test_build_reward_certs_from_votes_added_so_far() {
        let reward_slot = 100;
        let bank_slot = reward_slot + NUM_SLOTS_FOR_REWARD;
        let keypairs = get_keypairs(4, reward_slot);
        let builder = Mutex::<CertsBuilder>::default();
        let add_notar_vote = |slot, rank| {
            let vote = Vote::new_notarization_vote(Block {
                slot,
                block_id: BlockId::default(),
            });
            let (aggregate, pubkeys) = new_reward_vote_aggregate(vote, rank, &keypairs, None, 0);
            builder
                .lock()
                .unwrap()
                .add_aggregate(slot, keypairs.len(), aggregate, pubkeys)
                .unwrap();
        };
        add_notar_vote(reward_slot - 1, 0);
        for rank in 0..3 {
            add_notar_vote(reward_slot, rank);
        }

        // The footer gets every vote added so far, without asking ahead of time.
        let mut slot_metrics = SlotMetrics::new(bank_slot, false);
        let certs = build_reward_certs(&builder, bank_slot, &mut slot_metrics).unwrap();
        assert!(certs.notar.is_some());
        assert_eq!(certs.validators.len(), 3);

        // The reward slot's votes are taken, and older slots are dropped.
        assert!(builder.lock().unwrap().aggregates.is_empty());
        assert_eq!(
            build_reward_certs(&builder, bank_slot, &mut slot_metrics).unwrap(),
            RewardRespSucc::default()
        );
    }
}
