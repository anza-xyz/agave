use {
    crate::block_creation_loop::rewards::msg_types::RewardRespSucc,
    agave_bls_sigverify::rewards::RewardInput,
    agave_votor_messages::reward_certificate::{BuildRewardCertsRespError, NUM_SLOTS_FOR_REWARD},
    entry::Entry,
    solana_clock::Slot,
    solana_runtime::bank::Bank,
    std::collections::BTreeMap,
};

mod entry;
#[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
pub(super) mod recovery;

#[derive(Default)]
#[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
pub(super) struct InputStats {
    pub(super) purged_state: u64,
    pub(super) late_vote_aggregates: u64,
}

/// Container to store state needed to generate reward certificates.
#[derive(Default)]
#[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
pub(super) struct CertsBuilder {
    /// Per [`Slot`], stores the skip and notar votes.
    aggregates: BTreeMap<Slot, Entry>,
    /// A sealed slot must not be recreated by a late vote or worker completion.
    sealed_through: Option<Slot>,
}

impl CertsBuilder {
    /// Constructs a new instance of [`CertsBuilder`].
    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Builds reward certificates.
    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    pub(super) fn build_certs(
        &mut self,
        bank_slot: Slot,
    ) -> Result<RewardRespSucc, BuildRewardCertsRespError> {
        let Some(reward_slot) = bank_slot.checked_sub(NUM_SLOTS_FOR_REWARD) else {
            return Ok(RewardRespSucc::default());
        };
        self.sealed_through = Some(
            self.sealed_through
                .map_or(reward_slot, |slot| slot.max(reward_slot)),
        );
        // we assume that the block creation loop will only ever request to build reward certs in a
        // strictly increasing order so we can drop older state
        self.aggregates = self.aggregates.split_off(&reward_slot);
        self.aggregates.remove(&reward_slot).map_or_else(
            || Ok(RewardRespSucc::default()),
            |entry| entry.build_certs(reward_slot),
        )
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    pub(super) fn handle_input(&mut self, root_bank: &Bank, input: &RewardInput) -> InputStats {
        let mut stats = InputStats::default();
        let root_slot = root_bank.slot();
        let minimum_slot = root_slot
            .checked_sub(NUM_SLOTS_FOR_REWARD)
            .map_or(0, |slot| slot.saturating_add(1))
            .max(self.sealed_through.map_or(0, |slot| slot.saturating_add(1)));
        // drop state that is too old based on how the root slot has progressed
        // if this actually drops state, that probably indicates that the leader missed its
        // window.
        let new_aggregates = self.aggregates.split_off(&minimum_slot);
        if !self.aggregates.is_empty() {
            stats.purged_state += 1;
        }
        self.aggregates = new_aggregates;

        match input {
            RewardInput::External(aggregates) => {
                for aggregate in aggregates {
                    let vote = *aggregate.vote();
                    let vote_slot = vote.slot();
                    if vote_slot < minimum_slot {
                        stats.late_vote_aggregates += 1;
                        continue;
                    }
                    let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                        warn!(
                            "failed to look up rank_map for slot {vote_slot} using bank for slot \
                             {}",
                            root_bank.slot()
                        );
                        return stats;
                    };
                    let max_validators = rank_map.len();
                    let mut vote_account_pubkeys = vec![];
                    for rank in aggregate.ranks().iter_ones() {
                        let Some(stake_entry) = rank_map.get_pubkey_stake_entry(rank) else {
                            return stats;
                        };
                        vote_account_pubkeys.push(stake_entry.vote_account_pubkey);
                    }

                    match self
                        .aggregates
                        .entry(vote_slot)
                        .or_insert_with(|| Entry::new(max_validators))
                        .add_aggregate(aggregate, vote_account_pubkeys)
                    {
                        Ok(()) => (),
                        Err(e) => {
                            warn!("Adding aggregate with vote {vote:?} failed with {e}");
                        }
                    }
                }
            }
            RewardInput::Own(vote_msg) => {
                let vote = vote_msg.vote;
                let vote_slot = vote.slot();
                if vote_slot < minimum_slot {
                    stats.late_vote_aggregates += 1;
                    return stats;
                }
                let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                    warn!(
                        "failed to look up rank_map for slot {vote_slot} using bank for slot {}",
                        root_bank.slot()
                    );
                    return stats;
                };
                let max_validators = rank_map.len();
                let Some(stake_entry) = rank_map.get_pubkey_stake_entry(vote_msg.rank as usize)
                else {
                    return stats;
                };

                match self
                    .aggregates
                    .entry(vote_msg.vote.slot())
                    .or_insert_with(|| Entry::new(max_validators))
                    .add_own_msg(vote_msg, stake_entry.vote_account_pubkey)
                {
                    Ok(()) => (),
                    Err(e) => {
                        warn!("Adding aggregate with vote {vote:?} failed with {e}");
                    }
                }
            }
            RewardInput::Certificates(_) => (),
        }
        stats
    }
}
