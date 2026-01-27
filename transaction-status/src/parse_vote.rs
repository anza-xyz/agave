use {
    crate::parse_instruction::{
        check_num_accounts, ParsableProgram, ParseInstructionError, ParsedInstructionEnum,
    },
    bincode::deserialize,
    serde_json::json,
    solana_message::{compiled_instruction::CompiledInstruction, AccountKeys},
    solana_vote_interface::instruction::VoteInstruction,
};

pub fn parse_vote(
    instruction: &CompiledInstruction,
    account_keys: &AccountKeys,
) -> Result<ParsedInstructionEnum, ParseInstructionError> {
    let vote_instruction: VoteInstruction = deserialize(&instruction.data)
        .map_err(|_| ParseInstructionError::InstructionNotParsable(ParsableProgram::Vote))?;
    match instruction.accounts.iter().max() {
        Some(index) if (*index as usize) < account_keys.len() => {}
        _ => {
            // Runtime should prevent this from ever happening
            return Err(ParseInstructionError::InstructionKeyMismatch(
                ParsableProgram::Vote,
            ));
        }
    }
    match vote_instruction {
        VoteInstruction::InitializeAccount(vote_init) => {
            check_num_vote_accounts(&instruction.accounts, 4)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "initialize".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "rentSysvar": account_keys[instruction.accounts[1] as usize].to_string(),
                    "clockSysvar": account_keys[instruction.accounts[2] as usize].to_string(),
                    "node": account_keys[instruction.accounts[3] as usize].to_string(),
                    "authorizedVoter": vote_init.authorized_voter.to_string(),
                    "authorizedWithdrawer": vote_init.authorized_withdrawer.to_string(),
                    "commission": vote_init.commission,
                }),
            })
        }
        VoteInstruction::Authorize(new_authorized, authority_type) => {
            check_num_vote_accounts(&instruction.accounts, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "authorize".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "clockSysvar": account_keys[instruction.accounts[1] as usize].to_string(),
                    "authority": account_keys[instruction.accounts[2] as usize].to_string(),
                    "newAuthority": new_authorized.to_string(),
                    "authorityType": authority_type,
                }),
            })
        }
        VoteInstruction::AuthorizeWithSeed(args) => {
            check_num_vote_accounts(&instruction.accounts, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "authorizeWithSeed".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "clockSysvar": account_keys[instruction.accounts[1] as usize].to_string(),
                    "authorityBaseKey": account_keys[instruction.accounts[2] as usize].to_string(),
                    "authorityOwner": args.current_authority_derived_key_owner.to_string(),
                    "authoritySeed": args.current_authority_derived_key_seed,
                    "newAuthority": args.new_authority.to_string(),
                    "authorityType": args.authorization_type,
                }),
            })
        }
        VoteInstruction::AuthorizeCheckedWithSeed(args) => {
            check_num_vote_accounts(&instruction.accounts, 4)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "authorizeCheckedWithSeed".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "clockSysvar": account_keys[instruction.accounts[1] as usize].to_string(),
                    "authorityBaseKey": account_keys[instruction.accounts[2] as usize].to_string(),
                    "authorityOwner": args.current_authority_derived_key_owner.to_string(),
                    "authoritySeed": args.current_authority_derived_key_seed,
                    "newAuthority": account_keys[instruction.accounts[3] as usize].to_string(),
                    "authorityType": args.authorization_type,
                }),
            })
        }
        VoteInstruction::Vote(vote) => {
            check_num_vote_accounts(&instruction.accounts, 4)?;
            let vote = json!({
                "slots": vote.slots,
                "hash": vote.hash.to_string(),
                "timestamp": vote.timestamp,
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "vote".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "slotHashesSysvar": account_keys[instruction.accounts[1] as usize].to_string(),
                    "clockSysvar": account_keys[instruction.accounts[2] as usize].to_string(),
                    "voteAuthority": account_keys[instruction.accounts[3] as usize].to_string(),
                    "vote": vote,
                }),
            })
        }
        VoteInstruction::UpdateVoteState(vote_state_update) => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            let vote_state_update = json!({
                "lockouts": vote_state_update.lockouts,
                "root": vote_state_update.root,
                "hash": vote_state_update.hash.to_string(),
                "timestamp": vote_state_update.timestamp,
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "updatevotestate".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "voteAuthority": account_keys[instruction.accounts[1] as usize].to_string(),
                    "voteStateUpdate": vote_state_update,
                }),
            })
        }
        VoteInstruction::UpdateVoteStateSwitch(vote_state_update, hash) => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            let vote_state_update = json!({
                "lockouts": vote_state_update.lockouts,
                "root": vote_state_update.root,
                "hash": vote_state_update.hash.to_string(),
                "timestamp": vote_state_update.timestamp,
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "updatevotestateswitch".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "voteAuthority": account_keys[instruction.accounts[1] as usize].to_string(),
                    "voteStateUpdate": vote_state_update,
                    "hash": hash.to_string(),
                }),
            })
        }
        VoteInstruction::CompactUpdateVoteState(vote_state_update) => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            let vote_state_update = json!({
                "lockouts": vote_state_update.lockouts,
                "root": vote_state_update.root,
                "hash": vote_state_update.hash.to_string(),
                "timestamp": vote_state_update.timestamp,
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "compactupdatevotestate".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "voteAuthority": account_keys[instruction.accounts[1] as usize].to_string(),
                    "voteStateUpdate": vote_state_update,
                }),
            })
        }
        VoteInstruction::CompactUpdateVoteStateSwitch(vote_state_update, hash) => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            let vote_state_update = json!({
                "lockouts": vote_state_update.lockouts,
                "root": vote_state_update.root,
                "hash": vote_state_update.hash.to_string(),
                "timestamp": vote_state_update.timestamp,
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "compactupdatevotestateswitch".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "voteAuthority": account_keys[instruction.accounts[1] as usize].to_string(),
                    "voteStateUpdate": vote_state_update,
                    "hash": hash.to_string(),
                }),
            })
        }
        VoteInstruction::TowerSync(tower_sync) => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            let tower_sync = json!({
                "lockouts": tower_sync.lockouts,
                "root": tower_sync.root,
                "hash": tower_sync.hash.to_string(),
                "timestamp": tower_sync.timestamp,
                "blockId": tower_sync.block_id.to_string(),
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "towersync".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "voteAuthority": account_keys[instruction.accounts[1] as usize].to_string(),
                    "towerSync": tower_sync,
                }),
            })
        }
        VoteInstruction::TowerSyncSwitch(tower_sync, hash) => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            let tower_sync = json!({
                "lockouts": tower_sync.lockouts,
                "root": tower_sync.root,
                "hash": tower_sync.hash.to_string(),
                "timestamp": tower_sync.timestamp,
                "blockId": tower_sync.block_id.to_string(),
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "towersyncswitch".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "voteAuthority": account_keys[instruction.accounts[1] as usize].to_string(),
                    "towerSync": tower_sync,
                    "hash": hash.to_string(),
                }),
            })
        }
        VoteInstruction::Withdraw(lamports) => {
            check_num_vote_accounts(&instruction.accounts, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "withdraw".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "destination": account_keys[instruction.accounts[1] as usize].to_string(),
                    "withdrawAuthority": account_keys[instruction.accounts[2] as usize].to_string(),
                    "lamports": lamports,
                }),
            })
        }
        VoteInstruction::UpdateValidatorIdentity => {
            check_num_vote_accounts(&instruction.accounts, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "updateValidatorIdentity".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "newValidatorIdentity": account_keys[instruction.accounts[1] as usize].to_string(),
                    "withdrawAuthority": account_keys[instruction.accounts[2] as usize].to_string(),
                }),
            })
        }
        VoteInstruction::UpdateCommission(commission) => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "updateCommission".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "withdrawAuthority": account_keys[instruction.accounts[1] as usize].to_string(),
                    "commission": commission,
                }),
            })
        }
        VoteInstruction::VoteSwitch(vote, hash) => {
            check_num_vote_accounts(&instruction.accounts, 4)?;
            let vote = json!({
                "slots": vote.slots,
                "hash": vote.hash.to_string(),
                "timestamp": vote.timestamp,
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "voteSwitch".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "slotHashesSysvar": account_keys[instruction.accounts[1] as usize].to_string(),
                    "clockSysvar": account_keys[instruction.accounts[2] as usize].to_string(),
                    "voteAuthority": account_keys[instruction.accounts[3] as usize].to_string(),
                    "vote": vote,
                    "hash": hash.to_string(),
                }),
            })
        }
        VoteInstruction::AuthorizeChecked(authority_type) => {
            check_num_vote_accounts(&instruction.accounts, 4)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "authorizeChecked".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "clockSysvar": account_keys[instruction.accounts[1] as usize].to_string(),
                    "authority": account_keys[instruction.accounts[2] as usize].to_string(),
                    "newAuthority": account_keys[instruction.accounts[3] as usize].to_string(),
                    "authorityType": authority_type,
                }),
            })
        }
        VoteInstruction::InitializeAccountV2(vote_init) => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "initializeV2".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "node": account_keys[instruction.accounts[1] as usize].to_string(),
                    "authorizedVoter": vote_init.authorized_voter.to_string(),
                    "authorizedWithdrawer": vote_init.authorized_withdrawer.to_string(),
                    "inflationRewardsCommissionBps": vote_init.inflation_rewards_commission_bps,
                    "inflationRewardsCollector": vote_init.inflation_rewards_collector.to_string(),
                    "blockRevenueCommissionBps": vote_init.block_revenue_commission_bps,
                    "blockRevenueCollector": vote_init.block_revenue_collector.to_string(),
                }),
            })
        }
        VoteInstruction::UpdateCommissionCollector(kind) => {
            check_num_vote_accounts(&instruction.accounts, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "updateCommissionCollector".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "newCollector": account_keys[instruction.accounts[1] as usize].to_string(),
                    "withdrawAuthority": account_keys[instruction.accounts[2] as usize].to_string(),
                    "commissionKind": kind,
                }),
            })
        }
        VoteInstruction::UpdateCommissionBps {
            commission_bps,
            kind,
        } => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "updateCommissionBps".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "withdrawAuthority": account_keys[instruction.accounts[1] as usize].to_string(),
                    "commissionBps": commission_bps,
                    "commissionKind": kind,
                }),
            })
        }
        VoteInstruction::DepositDelegatorRewards { deposit } => {
            check_num_vote_accounts(&instruction.accounts, 2)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "depositDelegatorRewards".to_string(),
                info: json!({
                    "voteAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "source": account_keys[instruction.accounts[1] as usize].to_string(),
                    "deposit": deposit,
                }),
            })
        }
    }
}

fn check_num_vote_accounts(accounts: &[u8], num: usize) -> Result<(), ParseInstructionError> {
    check_num_accounts(accounts, num, ParsableProgram::Vote)
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_hash::Hash,
        solana_message::Message,
        solana_pubkey::Pubkey,
        solana_sdk_ids::sysvar,
        solana_vote_interface::{
            instruction::{self as vote_instruction, CommissionKind},
            state::{
                TowerSync, Vote, VoteAuthorize, VoteInit, VoteInitV2, VoteStateUpdate, VoteStateV4,
            },
        },
    };

    #[test]
    fn test_parse_vote_initialize_ix() {
        let lamports = 55;

        let commission = 10;
        let node_pubkey = Pubkey::new_unique();
        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter = Pubkey::new_unique();
        let authorized_withdrawer = Pubkey::new_unique();
        let vote_init = VoteInit {
            node_pubkey,
            authorized_voter,
            authorized_withdrawer,
            commission,
        };

        let instructions = vote_instruction::create_account_with_config(
            &Pubkey::new_unique(),
            &vote_pubkey,
            &vote_init,
            lamports,
            vote_instruction::CreateVoteAccountConfig {
                space: VoteStateV4::size_of() as u64,
                ..vote_instruction::CreateVoteAccountConfig::default()
            },
        );
        let mut message = Message::new(&instructions, None);
        assert_eq!(
            parse_vote(
                &message.instructions[1],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "initialize".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "rentSysvar": sysvar::rent::ID.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "node": node_pubkey.to_string(),
                    "authorizedVoter": authorized_voter.to_string(),
                    "authorizedWithdrawer": authorized_withdrawer.to_string(),
                    "commission": commission,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[1],
            &AccountKeys::new(&message.account_keys[0..3], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_authorize_ix() {
        let vote_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let authority_type = VoteAuthorize::Voter;
        let instruction = vote_instruction::authorize(
            &vote_pubkey,
            &authorized_pubkey,
            &new_authorized_pubkey,
            authority_type,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorize".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "authority": authorized_pubkey.to_string(),
                    "newAuthority": new_authorized_pubkey.to_string(),
                    "authorityType": authority_type,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..2], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_authorize_with_seed_ix() {
        let vote_pubkey = Pubkey::new_unique();
        let authorized_base_key = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let authority_type = VoteAuthorize::Voter;
        let current_authority_owner = Pubkey::new_unique();
        let current_authority_seed = "AUTHORITY_SEED";
        let instruction = vote_instruction::authorize_with_seed(
            &vote_pubkey,
            &authorized_base_key,
            &current_authority_owner,
            current_authority_seed,
            &new_authorized_pubkey,
            authority_type,
        );
        let message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorizeWithSeed".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "authorityBaseKey": authorized_base_key.to_string(),
                    "authorityOwner": current_authority_owner.to_string(),
                    "authoritySeed": current_authority_seed,
                    "newAuthority": new_authorized_pubkey.to_string(),
                    "authorityType": authority_type,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..2], None)
        )
        .is_err());
    }

    #[test]
    fn test_parse_vote_authorize_with_seed_checked_ix() {
        let vote_pubkey = Pubkey::new_unique();
        let authorized_base_key = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let authority_type = VoteAuthorize::Voter;
        let current_authority_owner = Pubkey::new_unique();
        let current_authority_seed = "AUTHORITY_SEED";
        let instruction = vote_instruction::authorize_checked_with_seed(
            &vote_pubkey,
            &authorized_base_key,
            &current_authority_owner,
            current_authority_seed,
            &new_authorized_pubkey,
            authority_type,
        );
        let message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorizeCheckedWithSeed".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "authorityBaseKey": authorized_base_key.to_string(),
                    "authorityOwner": current_authority_owner.to_string(),
                    "authoritySeed": current_authority_seed,
                    "newAuthority": new_authorized_pubkey.to_string(),
                    "authorityType": authority_type,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..3], None)
        )
        .is_err());
    }

    #[test]
    fn test_parse_vote_ix() {
        let hash = Hash::new_from_array([1; 32]);
        let vote = Vote {
            slots: vec![1, 2, 4],
            hash,
            timestamp: Some(1_234_567_890),
        };

        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter_pubkey = Pubkey::new_unique();
        let instruction = vote_instruction::vote(&vote_pubkey, &authorized_voter_pubkey, vote);
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "vote".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "slotHashesSysvar": sysvar::slot_hashes::ID.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "voteAuthority": authorized_voter_pubkey.to_string(),
                    "vote": {
                        "slots": [1, 2, 4],
                        "hash": hash.to_string(),
                        "timestamp": 1_234_567_890,
                    },
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..3], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_withdraw_ix() {
        let lamports = 55;
        let vote_pubkey = Pubkey::new_unique();
        let authorized_withdrawer_pubkey = Pubkey::new_unique();
        let to_pubkey = Pubkey::new_unique();
        let instruction = vote_instruction::withdraw(
            &vote_pubkey,
            &authorized_withdrawer_pubkey,
            lamports,
            &to_pubkey,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "withdraw".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "destination": to_pubkey.to_string(),
                    "withdrawAuthority": authorized_withdrawer_pubkey.to_string(),
                    "lamports": lamports,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..2], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_update_validator_identity_ix() {
        let vote_pubkey = Pubkey::new_unique();
        let authorized_withdrawer_pubkey = Pubkey::new_unique();
        let node_pubkey = Pubkey::new_unique();
        let instruction = vote_instruction::update_validator_identity(
            &vote_pubkey,
            &authorized_withdrawer_pubkey,
            &node_pubkey,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "updateValidatorIdentity".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "newValidatorIdentity": node_pubkey.to_string(),
                    "withdrawAuthority": authorized_withdrawer_pubkey.to_string(),
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..2], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_update_commission_ix() {
        let commission = 10;
        let vote_pubkey = Pubkey::new_unique();
        let authorized_withdrawer_pubkey = Pubkey::new_unique();
        let instruction = vote_instruction::update_commission(
            &vote_pubkey,
            &authorized_withdrawer_pubkey,
            commission,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "updateCommission".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "withdrawAuthority": authorized_withdrawer_pubkey.to_string(),
                    "commission": commission,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_switch_ix() {
        let hash = Hash::new_from_array([1; 32]);
        let vote = Vote {
            slots: vec![1, 2, 4],
            hash,
            timestamp: Some(1_234_567_890),
        };

        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter_pubkey = Pubkey::new_unique();
        let proof_hash = Hash::new_from_array([2; 32]);
        let instruction =
            vote_instruction::vote_switch(&vote_pubkey, &authorized_voter_pubkey, vote, proof_hash);
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "voteSwitch".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "slotHashesSysvar": sysvar::slot_hashes::ID.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "voteAuthority": authorized_voter_pubkey.to_string(),
                    "vote": {
                        "slots": [1, 2, 4],
                        "hash": hash.to_string(),
                        "timestamp": 1_234_567_890,
                    },
                    "hash": proof_hash.to_string(),
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..3], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_authorized_checked_ix() {
        let vote_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let authority_type = VoteAuthorize::Voter;
        let instruction = vote_instruction::authorize_checked(
            &vote_pubkey,
            &authorized_pubkey,
            &new_authorized_pubkey,
            authority_type,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorizeChecked".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "authority": authorized_pubkey.to_string(),
                    "newAuthority": new_authorized_pubkey.to_string(),
                    "authorityType": authority_type,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..3], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_state_update_ix() {
        let vote_state_update = VoteStateUpdate::from(vec![(0, 3), (1, 2), (2, 1)]);

        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter_pubkey = Pubkey::new_unique();
        let instruction = vote_instruction::update_vote_state(
            &vote_pubkey,
            &authorized_voter_pubkey,
            vote_state_update.clone(),
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "updatevotestate".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "voteAuthority": authorized_voter_pubkey.to_string(),
                    "voteStateUpdate": {
                        "lockouts": vote_state_update.lockouts,
                        "root": None::<u64>,
                        "hash": Hash::default().to_string(),
                        "timestamp": None::<u64>,
                    },
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_state_update_switch_ix() {
        let vote_state_update = VoteStateUpdate::from(vec![(0, 3), (1, 2), (2, 1)]);

        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter_pubkey = Pubkey::new_unique();
        let proof_hash = Hash::new_from_array([2; 32]);
        let instruction = vote_instruction::update_vote_state_switch(
            &vote_pubkey,
            &authorized_voter_pubkey,
            vote_state_update.clone(),
            proof_hash,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "updatevotestateswitch".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "voteAuthority": authorized_voter_pubkey.to_string(),
                    "voteStateUpdate": {
                        "lockouts": vote_state_update.lockouts,
                        "root": None::<u64>,
                        "hash": Hash::default().to_string(),
                        "timestamp": None::<u64>,
                    },
                    "hash": proof_hash.to_string(),
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_compact_vote_state_update_ix() {
        let vote_state_update = VoteStateUpdate::from(vec![(0, 3), (1, 2), (2, 1)]);
        let compact_vote_state_update = vote_state_update.clone();

        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter_pubkey = Pubkey::new_unique();
        let instruction = vote_instruction::compact_update_vote_state(
            &vote_pubkey,
            &authorized_voter_pubkey,
            compact_vote_state_update,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "compactupdatevotestate".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "voteAuthority": authorized_voter_pubkey.to_string(),
                    "voteStateUpdate": {
                        "lockouts": vote_state_update.lockouts,
                        "root": None::<u64>,
                        "hash": Hash::default().to_string(),
                        "timestamp": None::<u64>,
                    },
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_compact_vote_state_update_switch_ix() {
        let vote_state_update = VoteStateUpdate::from(vec![(0, 3), (1, 2), (2, 1)]);
        let compact_vote_state_update = vote_state_update.clone();

        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter_pubkey = Pubkey::new_unique();
        let proof_hash = Hash::new_from_array([2; 32]);
        let instruction = vote_instruction::compact_update_vote_state_switch(
            &vote_pubkey,
            &authorized_voter_pubkey,
            compact_vote_state_update,
            proof_hash,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "compactupdatevotestateswitch".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "voteAuthority": authorized_voter_pubkey.to_string(),
                    "voteStateUpdate": {
                        "lockouts": vote_state_update.lockouts,
                        "root": None::<u64>,
                        "hash": Hash::default().to_string(),
                        "timestamp": None::<u64>,
                    },
                    "hash": proof_hash.to_string(),
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_tower_sync_ix() {
        let tower_sync = TowerSync::from(vec![(0, 3), (1, 2), (2, 1)]);

        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter_pubkey = Pubkey::new_unique();
        let instruction = vote_instruction::tower_sync(
            &vote_pubkey,
            &authorized_voter_pubkey,
            tower_sync.clone(),
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "towersync".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "voteAuthority": authorized_voter_pubkey.to_string(),
                    "towerSync": {
                        "lockouts": tower_sync.lockouts,
                        "root": None::<u64>,
                        "hash": Hash::default().to_string(),
                        "timestamp": None::<u64>,
                        "blockId": Hash::default().to_string(),
                    },
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_tower_sync_switch_ix() {
        let tower_sync = TowerSync::from(vec![(0, 3), (1, 2), (2, 1)]);

        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter_pubkey = Pubkey::new_unique();
        let proof_hash = Hash::new_from_array([2; 32]);
        let instruction = vote_instruction::tower_sync_switch(
            &vote_pubkey,
            &authorized_voter_pubkey,
            tower_sync.clone(),
            proof_hash,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "towersyncswitch".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "voteAuthority": authorized_voter_pubkey.to_string(),
                    "towerSync": {
                        "lockouts": tower_sync.lockouts,
                        "root": None::<u64>,
                        "hash": Hash::default().to_string(),
                        "timestamp": None::<u64>,
                        "blockId": Hash::default().to_string(),
                    },
                    "hash": proof_hash.to_string(),
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_initialize_v2_ix() {
        let lamports = 55;

        let node_pubkey = Pubkey::new_unique();
        let vote_pubkey = Pubkey::new_unique();
        let authorized_voter = Pubkey::new_unique();
        let authorized_withdrawer = Pubkey::new_unique();
        let inflation_rewards_collector = Pubkey::new_unique();
        let block_revenue_collector = Pubkey::new_unique();
        let vote_init = VoteInitV2 {
            node_pubkey,
            authorized_voter,
            authorized_voter_bls_pubkey: [0u8; 48],
            authorized_voter_bls_proof_of_possession: [0u8; 96],
            authorized_withdrawer,
            inflation_rewards_commission_bps: 500,
            inflation_rewards_collector,
            block_revenue_commission_bps: 1000,
            block_revenue_collector,
        };

        let instructions = vote_instruction::create_account_with_config_v2(
            &Pubkey::new_unique(),
            &vote_pubkey,
            &vote_init,
            lamports,
            vote_instruction::CreateVoteAccountConfig {
                space: VoteStateV4::size_of() as u64,
                ..vote_instruction::CreateVoteAccountConfig::default()
            },
        );
        let mut message = Message::new(&instructions, None);
        assert_eq!(
            parse_vote(
                &message.instructions[1],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "initializeV2".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "node": node_pubkey.to_string(),
                    "authorizedVoter": authorized_voter.to_string(),
                    "authorizedWithdrawer": authorized_withdrawer.to_string(),
                    "inflationRewardsCommissionBps": 500,
                    "inflationRewardsCollector": inflation_rewards_collector.to_string(),
                    "blockRevenueCommissionBps": 1000,
                    "blockRevenueCollector": block_revenue_collector.to_string(),
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[1],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_update_commission_collector_ix() {
        let vote_pubkey = Pubkey::new_unique();
        let authorized_withdrawer_pubkey = Pubkey::new_unique();
        let new_collector_pubkey = Pubkey::new_unique();
        let instruction = vote_instruction::update_commission_collector(
            &vote_pubkey,
            &authorized_withdrawer_pubkey,
            &new_collector_pubkey,
            CommissionKind::InflationRewards,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "updateCommissionCollector".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "newCollector": new_collector_pubkey.to_string(),
                    "withdrawAuthority": authorized_withdrawer_pubkey.to_string(),
                    "commissionKind": CommissionKind::InflationRewards,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..2], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_update_commission_bps_ix() {
        let vote_pubkey = Pubkey::new_unique();
        let authorized_withdrawer_pubkey = Pubkey::new_unique();
        let commission_bps = 500;
        let instruction = vote_instruction::update_commission_bps(
            &vote_pubkey,
            &authorized_withdrawer_pubkey,
            CommissionKind::BlockRevenue,
            commission_bps,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "updateCommissionBps".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "withdrawAuthority": authorized_withdrawer_pubkey.to_string(),
                    "commissionBps": commission_bps,
                    "commissionKind": CommissionKind::BlockRevenue,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_vote_deposit_delegator_rewards_ix() {
        let vote_pubkey = Pubkey::new_unique();
        let source_pubkey = Pubkey::new_unique();
        let deposit = 1_000_000;
        let instruction =
            vote_instruction::deposit_delegator_rewards(&vote_pubkey, &source_pubkey, deposit);
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_vote(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "depositDelegatorRewards".to_string(),
                info: json!({
                    "voteAccount": vote_pubkey.to_string(),
                    "source": source_pubkey.to_string(),
                    "deposit": deposit,
                }),
            }
        );
        assert!(parse_vote(
            &message.instructions[0],
            &AccountKeys::new(&message.account_keys[0..1], None)
        )
        .is_err());
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_vote(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }
}
