use {
    crate::{parse_account_data::ParseAccountError, StringAmount},
    serde::{Deserialize, Serialize},
    solana_clock::{Epoch, Slot},
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_vote_interface::{
        authorized_voters::AuthorizedVoters,
        state::{BlockTimestamp, CircBuf, LandedVote, Lockout, VoteStateVersions},
    },
    std::collections::VecDeque,
};

fn convert_epoch_credits(epoch_credits: &[(Epoch, u64, u64)]) -> Vec<UiEpochCredits> {
    epoch_credits
        .iter()
        .map(|(epoch, credits, previous_credits)| UiEpochCredits {
            epoch: *epoch,
            credits: credits.to_string(),
            previous_credits: previous_credits.to_string(),
        })
        .collect()
}

fn convert_votes<T>(votes: &VecDeque<T>) -> Vec<UiLockout>
where
    for<'a> &'a T: Into<UiLockout>,
{
    votes.iter().map(Into::into).collect()
}

fn convert_authorized_voters(authorized_voters: &AuthorizedVoters) -> Vec<UiAuthorizedVoters> {
    authorized_voters
        .iter()
        .map(|(epoch, authorized_voter)| UiAuthorizedVoters {
            epoch: *epoch,
            authorized_voter: authorized_voter.to_string(),
        })
        .collect()
}

fn convert_prior_voters(prior_voters: &CircBuf<(Pubkey, Epoch, Epoch)>) -> Vec<UiPriorVoters> {
    prior_voters
        .buf()
        .iter()
        .filter(|(pubkey, _, _)| pubkey != &Pubkey::default())
        .map(
            |(authorized_pubkey, epoch_of_last_authorized_switch, target_epoch)| UiPriorVoters {
                authorized_pubkey: authorized_pubkey.to_string(),
                epoch_of_last_authorized_switch: *epoch_of_last_authorized_switch,
                target_epoch: *target_epoch,
            },
        )
        .collect()
}

pub fn parse_vote(
    data: &[u8],
    _vote_pubkey: &Pubkey,
) -> Result<VoteAccountType, ParseAccountError> {
    let versioned: VoteStateVersions = bincode::deserialize(data)
        .map_err(|_| ParseAccountError::from(InstructionError::InvalidAccountData))?;

    let state = match versioned {
        VoteStateVersions::V0_23_5(vote_state) => {
            let epoch_credits = convert_epoch_credits(&vote_state.epoch_credits);
            let votes = convert_votes(&vote_state.votes);
            let authorized_voters = vec![UiAuthorizedVoters {
                epoch: vote_state.authorized_voter_epoch,
                authorized_voter: vote_state.authorized_voter.to_string(),
            }];
            let prior_voters = vote_state
                .prior_voters
                .buf
                .iter()
                .filter(|(pubkey, _, _, _)| pubkey != &Pubkey::default())
                .map(
                    |(authorized_pubkey, epoch_of_last_authorized_switch, target_epoch, _)| {
                        UiPriorVoters {
                            authorized_pubkey: authorized_pubkey.to_string(),
                            epoch_of_last_authorized_switch: *epoch_of_last_authorized_switch,
                            target_epoch: *target_epoch,
                        }
                    },
                )
                .collect();
            UiVoteState {
                node_pubkey: vote_state.node_pubkey.to_string(),
                authorized_withdrawer: vote_state.authorized_withdrawer.to_string(),
                commission: vote_state.commission,
                votes,
                root_slot: vote_state.root_slot,
                authorized_voters,
                prior_voters,
                epoch_credits,
                last_timestamp: vote_state.last_timestamp,
            }
        }
        VoteStateVersions::V1_14_11(vote_state) => {
            let epoch_credits = convert_epoch_credits(&vote_state.epoch_credits);
            let votes = convert_votes(&vote_state.votes);
            let authorized_voters = convert_authorized_voters(&vote_state.authorized_voters);
            let prior_voters = convert_prior_voters(&vote_state.prior_voters);
            UiVoteState {
                node_pubkey: vote_state.node_pubkey.to_string(),
                authorized_withdrawer: vote_state.authorized_withdrawer.to_string(),
                commission: vote_state.commission,
                votes,
                root_slot: vote_state.root_slot,
                authorized_voters,
                prior_voters,
                epoch_credits,
                last_timestamp: vote_state.last_timestamp,
            }
        }
        VoteStateVersions::V3(vote_state) => {
            let epoch_credits = convert_epoch_credits(&vote_state.epoch_credits);
            let votes = convert_votes(&vote_state.votes);
            let authorized_voters = convert_authorized_voters(&vote_state.authorized_voters);
            let prior_voters = convert_prior_voters(&vote_state.prior_voters);
            UiVoteState {
                node_pubkey: vote_state.node_pubkey.to_string(),
                authorized_withdrawer: vote_state.authorized_withdrawer.to_string(),
                commission: vote_state.commission,
                votes,
                root_slot: vote_state.root_slot,
                authorized_voters,
                prior_voters,
                epoch_credits,
                last_timestamp: vote_state.last_timestamp,
            }
        }
        VoteStateVersions::V4(vote_state) => {
            let epoch_credits = convert_epoch_credits(&vote_state.epoch_credits);
            let votes = convert_votes(&vote_state.votes);
            let authorized_voters = convert_authorized_voters(&vote_state.authorized_voters);
            // Convert basis points to percentage.
            let commission = (vote_state.inflation_rewards_commission_bps / 100) as u8;
            UiVoteState {
                node_pubkey: vote_state.node_pubkey.to_string(),
                authorized_withdrawer: vote_state.authorized_withdrawer.to_string(),
                commission,
                votes,
                root_slot: vote_state.root_slot,
                authorized_voters,
                prior_voters: Vec::new(), // <-- V4 doesn't have prior_voters
                epoch_credits,
                last_timestamp: vote_state.last_timestamp,
            }
        }
    };

    Ok(VoteAccountType::Vote(state))
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

impl From<&LandedVote> for UiLockout {
    fn from(landed_vote: &LandedVote) -> Self {
        Self {
            slot: landed_vote.slot(),
            confirmation_count: landed_vote.confirmation_count(),
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

        let versioned_0_23_5 = VoteStateVersions::V0_23_5(Box::default());
        let vote_account_data_0_23_5 = bincode::serialize(&versioned_0_23_5).unwrap();
        let expected_0_23_5 = UiVoteState {
            node_pubkey: Pubkey::default().to_string(),
            authorized_withdrawer: Pubkey::default().to_string(),
            authorized_voters: vec![UiAuthorizedVoters {
                epoch: 0,
                authorized_voter: Pubkey::default().to_string(),
            }],
            ..UiVoteState::default()
        };
        assert_eq!(
            parse_vote(&vote_account_data_0_23_5, &vote_pubkey).unwrap(),
            VoteAccountType::Vote(expected_0_23_5)
        );

        let versioned_1_14_11 = VoteStateVersions::V1_14_11(Box::default());
        let vote_account_data_1_14_11 = bincode::serialize(&versioned_1_14_11).unwrap();
        let expected_1_14_11 = UiVoteState {
            node_pubkey: Pubkey::default().to_string(),
            authorized_withdrawer: Pubkey::default().to_string(),
            ..UiVoteState::default()
        };
        assert_eq!(
            parse_vote(&vote_account_data_1_14_11, &vote_pubkey).unwrap(),
            VoteAccountType::Vote(expected_1_14_11)
        );

        let versioned_v3 = VoteStateVersions::V3(Box::default());
        let vote_account_data_v3 = bincode::serialize(&versioned_v3).unwrap();
        let expected_v3 = UiVoteState {
            node_pubkey: Pubkey::default().to_string(),
            authorized_withdrawer: Pubkey::default().to_string(),
            ..UiVoteState::default()
        };
        assert_eq!(
            parse_vote(&vote_account_data_v3, &vote_pubkey).unwrap(),
            VoteAccountType::Vote(expected_v3)
        );

        let versioned_v4 = VoteStateVersions::V4(Box::default());
        let vote_account_data_v4 = bincode::serialize(&versioned_v4).unwrap();
        let expected_v4 = UiVoteState {
            node_pubkey: Pubkey::default().to_string(),
            authorized_withdrawer: Pubkey::default().to_string(),
            ..UiVoteState::default()
        };
        assert_eq!(
            parse_vote(&vote_account_data_v4, &vote_pubkey).unwrap(),
            VoteAccountType::Vote(expected_v4)
        );

        let bad_data = vec![0; 4];
        assert!(parse_vote(&bad_data, &vote_pubkey).is_err());
    }
}
