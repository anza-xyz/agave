use {
    super::identity_keyed, solana_pubkey::Pubkey, solana_sdk::clock::Epoch,
    solana_vote::vote_account::VoteAccountsHashMap,
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub(super) struct LeaderSchedule {
    pub(super) slot_leader_vote_account_addresses: Vec<Pubkey>,
    // cached leader schedule keyed by validator identities created by mapping
    // vote account addresses to the validator identity designated at the time
    // of leader schedule generation. This is used to avoid the need to look up
    // the validator identity address for each slot.
    pub(super) identity_keyed_leader_schedule: identity_keyed::LeaderSchedule,
}

impl LeaderSchedule {
    pub(super) fn new(
        vote_accounts_map: &VoteAccountsHashMap,
        epoch: Epoch,
        len: u64,
        repeat: u64,
    ) -> Self {
        let keyed_stakes: Vec<_> = vote_accounts_map
            .iter()
            .map(|(vote_pubkey, (stake, _account))| (vote_pubkey, *stake))
            .collect();
        let slot_leader_vote_account_addresses =
            super::LeaderSchedule::stake_weighted_slot_leaders(keyed_stakes, epoch, len, repeat);

        let identity_keyed_leader_schedule = {
            struct SlotLeaderInfo<'a> {
                vote_account_address: &'a Pubkey,
                validator_identity_address: &'a Pubkey,
            }

            let default_pubkey = Pubkey::default();
            let mut current_slot_leader_info = SlotLeaderInfo {
                vote_account_address: &default_pubkey,
                validator_identity_address: &default_pubkey,
            };

            let slot_leaders: Vec<Pubkey> = slot_leader_vote_account_addresses
                .iter()
                .map(|vote_account_address| {
                    if vote_account_address != current_slot_leader_info.vote_account_address {
                        let validator_identity_address = vote_accounts_map
                            .get(vote_account_address)
                            .unwrap()
                            .1
                            .node_pubkey();
                        current_slot_leader_info = SlotLeaderInfo {
                            vote_account_address,
                            validator_identity_address,
                        };
                    }
                    *current_slot_leader_info.validator_identity_address
                })
                .collect();

            identity_keyed::LeaderSchedule::new_from_schedule(slot_leaders)
        };

        Self {
            slot_leader_vote_account_addresses,
            identity_keyed_leader_schedule,
        }
    }
}
