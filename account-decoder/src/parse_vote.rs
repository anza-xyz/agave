use {
    crate::{parse_account_data::ParseAccountError, StringAmount},
    serde::{Deserialize, Serialize},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_vote_interface::state::{BlockTimestamp, Lockout, VoteStateV4},
};

pub fn parse_vote(data: &[u8], vote_pubkey: &Pubkey) -> Result<VoteAccountType, ParseAccountError> {
    let vote_state =
        VoteStateV4::deserialize(data, vote_pubkey).map_err(ParseAccountError::from)?;
    let epoch_credits = vote_state
        .epoch_credits
        .iter()
        .map(|(epoch, credits, previous_credits)| UiEpochCredits {
            epoch: *epoch,
            credits: credits.to_string(),
            previous_credits: previous_credits.to_string(),
        })
        .collect();
    let votes = vote_state
        .votes
        .iter()
        .map(|landed_vote| UiLockout {
            slot: landed_vote.slot(),
            confirmation_count: landed_vote.confirmation_count(),
        })
        .collect();
    let authorized_voters = vote_state
        .authorized_voters
        .iter()
        .map(|(epoch, authorized_voter)| UiAuthorizedVoters {
            epoch: *epoch,
            authorized_voter: authorized_voter.to_string(),
        })
        .collect();
    Ok(VoteAccountType::Vote(UiVoteState {
        node_pubkey: vote_state.node_pubkey.to_string(),
        authorized_withdrawer: vote_state.authorized_withdrawer.to_string(),
        commission: (vote_state.inflation_rewards_commission_bps / 100) as u8,
        votes,
        root_slot: vote_state.root_slot,
        authorized_voters,
        prior_voters: Vec::new(), // <-- No `prior_voters` in v4
        epoch_credits,
        last_timestamp: vote_state.last_timestamp,
        inflation_rewards_commission_bps: vote_state.inflation_rewards_commission_bps,
        inflation_rewards_collector: vote_state.inflation_rewards_collector.to_string(),
        block_revenue_collector: vote_state.block_revenue_collector.to_string(),
        block_revenue_commission_bps: vote_state.block_revenue_commission_bps,
        pending_delegator_rewards: vote_state.pending_delegator_rewards.to_string(),
        bls_pubkey_compressed: vote_state
            .bls_pubkey_compressed
            .map(|bytes| bs58::encode(bytes).into_string()),
    }))
}

/// A wrapper enum for consistency across programs
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "type", content = "info")]
pub enum VoteAccountType {
    Vote(UiVoteState),
}

/// A duplicate representation of VoteState for pretty JSON serialization
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UiVoteState {
    node_pubkey: String,
    authorized_withdrawer: String,
    commission: u8,
    votes: Vec<UiLockout>,
    root_slot: Option<Slot>,
    authorized_voters: Vec<UiAuthorizedVoters>,
    prior_voters: Vec<UiPriorVoters>,
    epoch_credits: Vec<UiEpochCredits>,
    last_timestamp: BlockTimestamp,
    // Fields added with vote state v4 via SIMD-0185:
    inflation_rewards_commission_bps: u16,
    inflation_rewards_collector: String,
    block_revenue_collector: String,
    block_revenue_commission_bps: u16,
    pending_delegator_rewards: StringAmount,
    #[serde(skip_serializing_if = "Option::is_none")]
    bls_pubkey_compressed: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct UiLockout {
    slot: Slot,
    confirmation_count: u32,
}

impl From<&Lockout> for UiLockout {
    fn from(lockout: &Lockout) -> Self {
        Self {
            slot: lockout.slot(),
            confirmation_count: lockout.confirmation_count(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct UiAuthorizedVoters {
    epoch: Epoch,
    authorized_voter: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct UiPriorVoters {
    authorized_pubkey: String,
    epoch_of_last_authorized_switch: Epoch,
    target_epoch: Epoch,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct UiEpochCredits {
    epoch: Epoch,
    credits: StringAmount,
    previous_credits: StringAmount,
}

#[cfg(test)]
mod test {
    use {super::*, solana_vote_interface::state::VoteStateVersions};

    #[test]
    fn test_parse_vote() {
        let vote_pubkey = Pubkey::new_unique();
        let vote_state = VoteStateV4::default();
        let mut vote_account_data: Vec<u8> = vec![0; VoteStateV4::size_of()];
        let versioned = VoteStateVersions::new_v4(vote_state.clone());
        VoteStateV4::serialize(&versioned, &mut vote_account_data).unwrap();
        let expected_vote_state = UiVoteState {
            node_pubkey: Pubkey::default().to_string(),
            authorized_withdrawer: Pubkey::default().to_string(),
            commission: 0,
            votes: vec![],
            root_slot: None,
            authorized_voters: vec![],
            prior_voters: vec![],
            epoch_credits: vec![],
            last_timestamp: BlockTimestamp::default(),
            inflation_rewards_commission_bps: vote_state.inflation_rewards_commission_bps,
            inflation_rewards_collector: vote_state.inflation_rewards_collector.to_string(),
            block_revenue_collector: vote_state.block_revenue_collector.to_string(),
            block_revenue_commission_bps: vote_state.block_revenue_commission_bps,
            pending_delegator_rewards: vote_state.pending_delegator_rewards.to_string(),
            bls_pubkey_compressed: None,
        };
        assert_eq!(
            parse_vote(&vote_account_data, &vote_pubkey).unwrap(),
            VoteAccountType::Vote(expected_vote_state)
        );

        let bad_data = vec![0; 4];
        assert!(parse_vote(&bad_data, &vote_pubkey).is_err());
    }
}
