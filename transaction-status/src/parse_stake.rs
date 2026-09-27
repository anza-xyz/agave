use {
    crate::parse_instruction::{
        ParsableProgram, ParseInstructionError, ParsedInstructionEnum, check_num_accounts,
    },
    bincode::deserialize,
    serde_json::{Map, Value, json},
    solana_message::{AccountKeys, compiled_instruction::CompiledInstruction},
    solana_pubkey::Pubkey,
    solana_sdk_ids::sysvar,
    solana_stake_interface::instruction::StakeInstruction,
};

/// Whether the account at `index` is `expected`.
///
/// Stake instructions used to carry clock, rent, stake-history and stake-config
/// accounts that current builders omit. Both shapes are on chain, and the
/// account count alone does not separate them - an `Authorize` with a custodian
/// is the same length either way - so the dropped slots are recognised by key.
fn holds(
    instruction: &CompiledInstruction,
    account_keys: &AccountKeys,
    index: usize,
    expected: &Pubkey,
) -> bool {
    instruction
        .accounts
        .get(index)
        .is_some_and(|i| account_keys[*i as usize] == *expected)
}

pub fn parse_stake(
    instruction: &CompiledInstruction,
    account_keys: &AccountKeys,
) -> Result<ParsedInstructionEnum, ParseInstructionError> {
    let stake_instruction: StakeInstruction = deserialize(&instruction.data)
        .map_err(|_| ParseInstructionError::InstructionNotParsable(ParsableProgram::Stake))?;
    match instruction.accounts.iter().max() {
        Some(index) if (*index as usize) < account_keys.len() => {}
        _ => {
            // Runtime should prevent this from ever happening
            return Err(ParseInstructionError::InstructionKeyMismatch(
                ParsableProgram::Stake,
            ));
        }
    }
    match stake_instruction {
        StakeInstruction::Initialize(authorized, lockup) => {
            let legacy = holds(instruction, account_keys, 1, &sysvar::rent::id());
            check_num_stake_accounts(&instruction.accounts, if legacy { 2 } else { 1 })?;
            let authorized = json!({
                "staker": authorized.staker.to_string(),
                "withdrawer": authorized.withdrawer.to_string(),
            });
            let lockup = json!({
                "unixTimestamp": lockup.unix_timestamp,
                "epoch": lockup.epoch,
                "custodian": lockup.custodian.to_string(),
            });
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "authorized": authorized,
                "lockup": lockup,
            });
            if legacy {
                value.as_object_mut().unwrap().insert(
                    "rentSysvar".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "initialize".to_string(),
                info: value,
            })
        }
        StakeInstruction::Authorize(new_authorized, authority_type) => {
            let legacy = holds(instruction, account_keys, 1, &sysvar::clock::id());
            check_num_stake_accounts(&instruction.accounts, if legacy { 3 } else { 2 })?;
            let authority = if legacy { 2 } else { 1 };
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "authority": account_keys[instruction.accounts[authority] as usize].to_string(),
                "newAuthority": new_authorized.to_string(),
                "authorityType": authority_type,
            });
            let map = value.as_object_mut().unwrap();
            if legacy {
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
            }
            if instruction.accounts.len() > authority + 1 {
                map.insert(
                    "custodian".to_string(),
                    json!(account_keys[instruction.accounts[authority + 1] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "authorize".to_string(),
                info: value,
            })
        }
        StakeInstruction::DelegateStake => {
            let legacy = holds(instruction, account_keys, 2, &sysvar::clock::id());
            check_num_stake_accounts(&instruction.accounts, if legacy { 6 } else { 3 })?;
            let authority = if legacy { 5 } else { 2 };
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "voteAccount": account_keys[instruction.accounts[1] as usize].to_string(),
                "stakeAuthority": account_keys[instruction.accounts[authority] as usize].to_string(),
            });
            if legacy {
                let map = value.as_object_mut().unwrap();
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                map.insert(
                    "stakeHistorySysvar".to_string(),
                    json!(account_keys[instruction.accounts[3] as usize].to_string()),
                );
                map.insert(
                    "stakeConfigAccount".to_string(),
                    json!(account_keys[instruction.accounts[4] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "delegate".to_string(),
                info: value,
            })
        }
        StakeInstruction::Split(lamports) => {
            check_num_stake_accounts(&instruction.accounts, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "split".to_string(),
                info: json!({
                    "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "newSplitAccount": account_keys[instruction.accounts[1] as usize].to_string(),
                    "stakeAuthority": account_keys[instruction.accounts[2] as usize].to_string(),
                    "lamports": lamports,
                }),
            })
        }
        StakeInstruction::Withdraw(lamports) => {
            let legacy = holds(instruction, account_keys, 2, &sysvar::clock::id());
            check_num_stake_accounts(&instruction.accounts, if legacy { 5 } else { 3 })?;
            let authority = if legacy { 4 } else { 2 };
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "destination": account_keys[instruction.accounts[1] as usize].to_string(),
                "withdrawAuthority": account_keys[instruction.accounts[authority] as usize].to_string(),
                "lamports": lamports,
            });
            let map = value.as_object_mut().unwrap();
            if legacy {
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                map.insert(
                    "stakeHistorySysvar".to_string(),
                    json!(account_keys[instruction.accounts[3] as usize].to_string()),
                );
            }
            if instruction.accounts.len() > authority + 1 {
                map.insert(
                    "custodian".to_string(),
                    json!(account_keys[instruction.accounts[authority + 1] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "withdraw".to_string(),
                info: value,
            })
        }
        StakeInstruction::Deactivate => {
            let legacy = holds(instruction, account_keys, 1, &sysvar::clock::id());
            check_num_stake_accounts(&instruction.accounts, if legacy { 3 } else { 2 })?;
            let authority = if legacy { 2 } else { 1 };
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "stakeAuthority": account_keys[instruction.accounts[authority] as usize].to_string(),
            });
            if legacy {
                value.as_object_mut().unwrap().insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "deactivate".to_string(),
                info: value,
            })
        }
        StakeInstruction::SetLockup(lockup_args) => {
            check_num_stake_accounts(&instruction.accounts, 2)?;
            let mut lockup_map = Map::new();
            if let Some(timestamp) = lockup_args.unix_timestamp {
                lockup_map.insert("unixTimestamp".to_string(), json!(timestamp));
            }
            if let Some(epoch) = lockup_args.epoch {
                lockup_map.insert("epoch".to_string(), json!(epoch));
            }
            if let Some(custodian) = lockup_args.custodian {
                lockup_map.insert("custodian".to_string(), json!(custodian.to_string()));
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "setLockup".to_string(),
                info: json!({
                    "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "custodian": account_keys[instruction.accounts[1] as usize].to_string(),
                    "lockup": lockup_map,
                }),
            })
        }
        StakeInstruction::Merge => {
            let legacy = holds(instruction, account_keys, 2, &sysvar::clock::id());
            check_num_stake_accounts(&instruction.accounts, if legacy { 5 } else { 3 })?;
            let authority = if legacy { 4 } else { 2 };
            let mut value = json!({
                "destination": account_keys[instruction.accounts[0] as usize].to_string(),
                "source": account_keys[instruction.accounts[1] as usize].to_string(),
                "stakeAuthority": account_keys[instruction.accounts[authority] as usize].to_string(),
            });
            if legacy {
                let map = value.as_object_mut().unwrap();
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                map.insert(
                    "stakeHistorySysvar".to_string(),
                    json!(account_keys[instruction.accounts[3] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "merge".to_string(),
                info: value,
            })
        }
        StakeInstruction::AuthorizeWithSeed(args) => {
            check_num_stake_accounts(&instruction.accounts, 2)?;
            let mut value = json!({
                    "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "authorityBase": account_keys[instruction.accounts[1] as usize].to_string(),
                    "newAuthorized": args.new_authorized_pubkey.to_string(),
                    "authorityType": args.stake_authorize,
                    "authoritySeed": args.authority_seed,
                    "authorityOwner": args.authority_owner.to_string(),
            });
            let map = value.as_object_mut().unwrap();
            let legacy = holds(instruction, account_keys, 2, &sysvar::clock::id());
            if legacy {
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
            }
            let custodian = if legacy { 3 } else { 2 };
            if instruction.accounts.len() > custodian {
                map.insert(
                    "custodian".to_string(),
                    json!(account_keys[instruction.accounts[custodian] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "authorizeWithSeed".to_string(),
                info: value,
            })
        }
        StakeInstruction::InitializeChecked => {
            let legacy = holds(instruction, account_keys, 1, &sysvar::rent::id());
            check_num_stake_accounts(&instruction.accounts, if legacy { 4 } else { 3 })?;
            let staker = if legacy { 2 } else { 1 };
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "staker": account_keys[instruction.accounts[staker] as usize].to_string(),
                "withdrawer": account_keys[instruction.accounts[staker + 1] as usize].to_string(),
            });
            if legacy {
                value.as_object_mut().unwrap().insert(
                    "rentSysvar".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "initializeChecked".to_string(),
                info: value,
            })
        }
        StakeInstruction::AuthorizeChecked(authority_type) => {
            let legacy = holds(instruction, account_keys, 1, &sysvar::clock::id());
            check_num_stake_accounts(&instruction.accounts, if legacy { 4 } else { 3 })?;
            let authority = if legacy { 2 } else { 1 };
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "authority": account_keys[instruction.accounts[authority] as usize].to_string(),
                "newAuthority": account_keys[instruction.accounts[authority + 1] as usize].to_string(),
                "authorityType": authority_type,
            });
            let map = value.as_object_mut().unwrap();
            if legacy {
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
            }
            if instruction.accounts.len() > authority + 2 {
                map.insert(
                    "custodian".to_string(),
                    json!(account_keys[instruction.accounts[authority + 2] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "authorizeChecked".to_string(),
                info: value,
            })
        }
        StakeInstruction::AuthorizeCheckedWithSeed(args) => {
            let legacy = holds(instruction, account_keys, 2, &sysvar::clock::id());
            check_num_stake_accounts(&instruction.accounts, if legacy { 4 } else { 3 })?;
            let new_authorized = if legacy { 3 } else { 2 };
            let mut value = json!({
                    "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "authorityBase": account_keys[instruction.accounts[1] as usize].to_string(),
                    "newAuthorized": account_keys[instruction.accounts[new_authorized] as usize].to_string(),
                    "authorityType": args.stake_authorize,
                    "authoritySeed": args.authority_seed,
                    "authorityOwner": args.authority_owner.to_string(),
            });
            let map = value.as_object_mut().unwrap();
            if legacy {
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
            }
            if instruction.accounts.len() > new_authorized + 1 {
                map.insert(
                    "custodian".to_string(),
                    json!(
                        account_keys[instruction.accounts[new_authorized + 1] as usize].to_string()
                    ),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "authorizeCheckedWithSeed".to_string(),
                info: value,
            })
        }
        StakeInstruction::SetLockupChecked(lockup_args) => {
            check_num_stake_accounts(&instruction.accounts, 2)?;
            let mut lockup_map = Map::new();
            if let Some(timestamp) = lockup_args.unix_timestamp {
                lockup_map.insert("unixTimestamp".to_string(), json!(timestamp));
            }
            if let Some(epoch) = lockup_args.epoch {
                lockup_map.insert("epoch".to_string(), json!(epoch));
            }
            if instruction.accounts.len() >= 3 {
                lockup_map.insert(
                    "custodian".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "setLockupChecked".to_string(),
                info: json!({
                    "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "custodian": account_keys[instruction.accounts[1] as usize].to_string(),
                    "lockup": lockup_map,
                }),
            })
        }
        StakeInstruction::GetMinimumDelegation => Ok(ParsedInstructionEnum {
            instruction_type: "getMinimumDelegation".to_string(),
            info: Value::default(),
        }),
        StakeInstruction::DeactivateDelinquent => {
            check_num_stake_accounts(&instruction.accounts, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "deactivateDelinquent".to_string(),
                info: json!({
                    "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "voteAccount": account_keys[instruction.accounts[1] as usize].to_string(),
                    "referenceVoteAccount": account_keys[instruction.accounts[2] as usize].to_string(),
                }),
            })
        }
        #[allow(deprecated)]
        StakeInstruction::Redelegate => {
            check_num_stake_accounts(&instruction.accounts, 5)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "redelegate".to_string(),
                info: json!({
                    "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "newStakeAccount": account_keys[instruction.accounts[1] as usize].to_string(),
                    "voteAccount": account_keys[instruction.accounts[2] as usize].to_string(),
                    "stakeConfigAccount": account_keys[instruction.accounts[3] as usize].to_string(),
                    "stakeAuthority": account_keys[instruction.accounts[4] as usize].to_string(),
                }),
            })
        }
        StakeInstruction::MoveStake(lamports) => {
            check_num_stake_accounts(&instruction.accounts, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "moveStake".to_string(),
                info: json!({
                    "source": account_keys[instruction.accounts[0] as usize].to_string(),
                    "destination": account_keys[instruction.accounts[1] as usize].to_string(),
                    "stakeAuthority": account_keys[instruction.accounts[2] as usize].to_string(),
                    "lamports": lamports,
                }),
            })
        }
        StakeInstruction::MoveLamports(lamports) => {
            check_num_stake_accounts(&instruction.accounts, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "moveLamports".to_string(),
                info: json!({
                    "source": account_keys[instruction.accounts[0] as usize].to_string(),
                    "destination": account_keys[instruction.accounts[1] as usize].to_string(),
                    "stakeAuthority": account_keys[instruction.accounts[2] as usize].to_string(),
                    "lamports": lamports,
                }),
            })
        }
    }
}

fn check_num_stake_accounts(accounts: &[u8], num: usize) -> Result<(), ParseInstructionError> {
    check_num_accounts(accounts, num, ParsableProgram::Stake)
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_instruction::{AccountMeta, Instruction},
        solana_message::Message,
        solana_pubkey::Pubkey,
        solana_stake_interface::{
            config,
            instruction::{self, LockupArgs},
            state::{Authorized, Lockup, StakeAuthorize},
        },
        std::iter::repeat_with,
    };

    #[test]
    fn test_parse_stake_initialize_ix() {
        let from_pubkey = Pubkey::new_unique();
        let stake_pubkey = Pubkey::new_unique();
        let authorized = Authorized {
            staker: Pubkey::new_unique(),
            withdrawer: Pubkey::new_unique(),
        };
        let lockup = Lockup {
            unix_timestamp: 1_234_567_890,
            epoch: 11,
            custodian: Pubkey::new_unique(),
        };
        let lamports = 55;

        let instructions = instruction::create_account(
            &from_pubkey,
            &stake_pubkey,
            &authorized,
            &lockup,
            lamports,
        );
        let mut message = Message::new(&instructions, None);
        assert_eq!(
            parse_stake(
                &message.instructions[1],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "initialize".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "authorized": {
                        "staker": authorized.staker.to_string(),
                        "withdrawer": authorized.withdrawer.to_string(),
                    },
                    "lockup": {
                        "unixTimestamp": lockup.unix_timestamp,
                        "epoch": lockup.epoch,
                        "custodian": lockup.custodian.to_string(),
                    }
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[1],
                &AccountKeys::new(&message.account_keys[0..1], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_authorize_ix() {
        let stake_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let custodian_pubkey = Pubkey::new_unique();
        let instruction = instruction::authorize(
            &stake_pubkey,
            &authorized_pubkey,
            &new_authorized_pubkey,
            StakeAuthorize::Staker,
            None,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorize".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "authority": authorized_pubkey.to_string(),
                    "newAuthority": new_authorized_pubkey.to_string(),
                    "authorityType": StakeAuthorize::Staker,
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..1], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        let instruction = instruction::authorize(
            &stake_pubkey,
            &authorized_pubkey,
            &new_authorized_pubkey,
            StakeAuthorize::Withdrawer,
            Some(&custodian_pubkey),
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorize".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "authority": authorized_pubkey.to_string(),
                    "newAuthority": new_authorized_pubkey.to_string(),
                    "authorityType": StakeAuthorize::Withdrawer,
                    "custodian": custodian_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..1], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_delegate_ix() {
        let stake_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let vote_pubkey = Pubkey::new_unique();
        let instruction =
            instruction::delegate_stake(&stake_pubkey, &authorized_pubkey, &vote_pubkey);
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "delegate".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "voteAccount": vote_pubkey.to_string(),
                    "stakeAuthority": authorized_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_split_ix() {
        let lamports = 55;
        let stake_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let split_stake_pubkey = Pubkey::new_unique();
        let instructions = instruction::split(
            &stake_pubkey,
            &authorized_pubkey,
            lamports,
            &split_stake_pubkey,
        );
        let mut message = Message::new(&instructions, None);
        assert_eq!(
            parse_stake(
                &message.instructions[2],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "split".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "newSplitAccount": split_stake_pubkey.to_string(),
                    "stakeAuthority": authorized_pubkey.to_string(),
                    "lamports": lamports,
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[2],
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_withdraw_ix() {
        let lamports = 55;
        let stake_pubkey = Pubkey::new_unique();
        let withdrawer_pubkey = Pubkey::new_unique();
        let to_pubkey = Pubkey::new_unique();
        let custodian_pubkey = Pubkey::new_unique();
        let instruction = instruction::withdraw(
            &stake_pubkey,
            &withdrawer_pubkey,
            &to_pubkey,
            lamports,
            None,
        );
        let message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "withdraw".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "destination": to_pubkey.to_string(),
                    "withdrawAuthority": withdrawer_pubkey.to_string(),
                    "lamports": lamports,
                }),
            }
        );
        let instruction = instruction::withdraw(
            &stake_pubkey,
            &withdrawer_pubkey,
            &to_pubkey,
            lamports,
            Some(&custodian_pubkey),
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "withdraw".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "destination": to_pubkey.to_string(),
                    "withdrawAuthority": withdrawer_pubkey.to_string(),
                    "custodian": custodian_pubkey.to_string(),
                    "lamports": lamports,
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_deactivate_stake_ix() {
        let stake_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let instruction = instruction::deactivate_stake(&stake_pubkey, &authorized_pubkey);
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "deactivate".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "stakeAuthority": authorized_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..1], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_merge_ix() {
        let destination_stake_pubkey = Pubkey::new_unique();
        let source_stake_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let instructions = instruction::merge(
            &destination_stake_pubkey,
            &source_stake_pubkey,
            &authorized_pubkey,
        );
        let mut message = Message::new(&instructions, None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "merge".to_string(),
                info: json!({
                    "destination": destination_stake_pubkey.to_string(),
                    "source": source_stake_pubkey.to_string(),
                    "stakeAuthority": authorized_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_authorize_with_seed_ix() {
        let stake_pubkey = Pubkey::new_unique();
        let authority_base_pubkey = Pubkey::new_unique();
        let authority_owner_pubkey = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let custodian_pubkey = Pubkey::new_unique();

        let seed = "test_seed";
        let instruction = instruction::authorize_with_seed(
            &stake_pubkey,
            &authority_base_pubkey,
            seed.to_string(),
            &authority_owner_pubkey,
            &new_authorized_pubkey,
            StakeAuthorize::Staker,
            None,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorizeWithSeed".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "authorityOwner": authority_owner_pubkey.to_string(),
                    "newAuthorized": new_authorized_pubkey.to_string(),
                    "authorityBase": authority_base_pubkey.to_string(),
                    "authoritySeed": seed,
                    "authorityType": StakeAuthorize::Staker,
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..1], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        let instruction = instruction::authorize_with_seed(
            &stake_pubkey,
            &authority_base_pubkey,
            seed.to_string(),
            &authority_owner_pubkey,
            &new_authorized_pubkey,
            StakeAuthorize::Withdrawer,
            Some(&custodian_pubkey),
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorizeWithSeed".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "authorityOwner": authority_owner_pubkey.to_string(),
                    "newAuthorized": new_authorized_pubkey.to_string(),
                    "authorityBase": authority_base_pubkey.to_string(),
                    "authoritySeed": seed,
                    "authorityType": StakeAuthorize::Withdrawer,
                    "custodian": custodian_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_set_lockup() {
        let keys: Vec<Pubkey> = repeat_with(Pubkey::new_unique).take(3).collect();
        let unix_timestamp = 1_234_567_890;
        let epoch = 11;
        let custodian = Pubkey::new_unique();

        let lockup = LockupArgs {
            unix_timestamp: Some(unix_timestamp),
            epoch: None,
            custodian: None,
        };
        let instruction = instruction::set_lockup(&keys[1], &lockup, &keys[0]);
        let message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&keys[0..2], None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "setLockup".to_string(),
                info: json!({
                    "stakeAccount": keys[1].to_string(),
                    "custodian": keys[0].to_string(),
                    "lockup": {
                        "unixTimestamp": unix_timestamp
                    }
                }),
            }
        );

        let lockup = LockupArgs {
            unix_timestamp: Some(unix_timestamp),
            epoch: Some(epoch),
            custodian: None,
        };
        let instruction = instruction::set_lockup(&keys[1], &lockup, &keys[0]);
        let message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&keys[0..2], None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "setLockup".to_string(),
                info: json!({
                    "stakeAccount": keys[1].to_string(),
                    "custodian": keys[0].to_string(),
                    "lockup": {
                        "unixTimestamp": unix_timestamp,
                        "epoch": epoch,
                    }
                }),
            }
        );

        let lockup = LockupArgs {
            unix_timestamp: Some(unix_timestamp),
            epoch: Some(epoch),
            custodian: Some(custodian),
        };
        let instruction = instruction::set_lockup(&keys[1], &lockup, &keys[0]);
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&keys[0..2], None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "setLockup".to_string(),
                info: json!({
                    "stakeAccount": keys[1].to_string(),
                    "custodian": keys[0].to_string(),
                    "lockup": {
                        "unixTimestamp": unix_timestamp,
                        "epoch": epoch,
                        "custodian": custodian.to_string(),
                    }
                }),
            }
        );

        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&keys[0..1], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        let lockup = LockupArgs {
            unix_timestamp: Some(unix_timestamp),
            epoch: None,
            custodian: None,
        };
        let instruction = instruction::set_lockup_checked(&keys[1], &lockup, &keys[0]);
        let message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&keys[0..2], None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "setLockupChecked".to_string(),
                info: json!({
                    "stakeAccount": keys[1].to_string(),
                    "custodian": keys[0].to_string(),
                    "lockup": {
                        "unixTimestamp": unix_timestamp
                    }
                }),
            }
        );

        let lockup = LockupArgs {
            unix_timestamp: Some(unix_timestamp),
            epoch: Some(epoch),
            custodian: None,
        };
        let instruction = instruction::set_lockup_checked(&keys[1], &lockup, &keys[0]);
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&keys[0..2], None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "setLockupChecked".to_string(),
                info: json!({
                    "stakeAccount": keys[1].to_string(),
                    "custodian": keys[0].to_string(),
                    "lockup": {
                        "unixTimestamp": unix_timestamp,
                        "epoch": epoch,
                    }
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&keys[0..1], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        let lockup = LockupArgs {
            unix_timestamp: Some(unix_timestamp),
            epoch: Some(epoch),
            custodian: Some(keys[1]),
        };
        let instruction = instruction::set_lockup_checked(&keys[2], &lockup, &keys[0]);
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&keys[0..3], None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "setLockupChecked".to_string(),
                info: json!({
                    "stakeAccount": keys[2].to_string(),
                    "custodian": keys[0].to_string(),
                    "lockup": {
                        "unixTimestamp": unix_timestamp,
                        "epoch": epoch,
                        "custodian": keys[1].to_string(),
                    }
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_create_account_checked_ix() {
        let from_pubkey = Pubkey::new_unique();
        let stake_pubkey = Pubkey::new_unique();

        let authorized = Authorized {
            staker: Pubkey::new_unique(),
            withdrawer: Pubkey::new_unique(),
        };
        let lamports = 55;

        let instructions =
            instruction::create_account_checked(&from_pubkey, &stake_pubkey, &authorized, lamports);
        let mut message = Message::new(&instructions, None);
        assert_eq!(
            parse_stake(
                &message.instructions[1],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "initializeChecked".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "staker": authorized.staker.to_string(),
                    "withdrawer": authorized.withdrawer.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[1],
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_authorize_checked_ix() {
        let stake_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let custodian_pubkey = Pubkey::new_unique();

        let instruction = instruction::authorize_checked(
            &stake_pubkey,
            &authorized_pubkey,
            &new_authorized_pubkey,
            StakeAuthorize::Staker,
            None,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorizeChecked".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "authority": authorized_pubkey.to_string(),
                    "newAuthority": new_authorized_pubkey.to_string(),
                    "authorityType": StakeAuthorize::Staker,
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        let instruction = instruction::authorize_checked(
            &stake_pubkey,
            &authorized_pubkey,
            &new_authorized_pubkey,
            StakeAuthorize::Withdrawer,
            Some(&custodian_pubkey),
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorizeChecked".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "authority": authorized_pubkey.to_string(),
                    "newAuthority": new_authorized_pubkey.to_string(),
                    "authorityType": StakeAuthorize::Withdrawer,
                    "custodian": custodian_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..3], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_authorize_checked_with_seed_ix() {
        let stake_pubkey = Pubkey::new_unique();
        let authority_base_pubkey = Pubkey::new_unique();
        let authority_owner_pubkey = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let custodian_pubkey = Pubkey::new_unique();

        let seed = "test_seed";
        let instruction = instruction::authorize_checked_with_seed(
            &stake_pubkey,
            &authority_base_pubkey,
            seed.to_string(),
            &authority_owner_pubkey,
            &new_authorized_pubkey,
            StakeAuthorize::Staker,
            None,
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorizeCheckedWithSeed".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "authorityOwner": authority_owner_pubkey.to_string(),
                    "newAuthorized": new_authorized_pubkey.to_string(),
                    "authorityBase": authority_base_pubkey.to_string(),
                    "authoritySeed": seed,
                    "authorityType": StakeAuthorize::Staker,
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        let instruction = instruction::authorize_checked_with_seed(
            &stake_pubkey,
            &authority_base_pubkey,
            seed.to_string(),
            &authority_owner_pubkey,
            &new_authorized_pubkey,
            StakeAuthorize::Withdrawer,
            Some(&custodian_pubkey),
        );
        let mut message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "authorizeCheckedWithSeed".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "authorityOwner": authority_owner_pubkey.to_string(),
                    "newAuthorized": new_authorized_pubkey.to_string(),
                    "authorityBase": authority_base_pubkey.to_string(),
                    "authoritySeed": seed,
                    "authorityType": StakeAuthorize::Withdrawer,
                    "custodian": custodian_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..3], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
    }

    #[test]
    fn test_parse_stake_move_ix() {
        let source_stake_pubkey = Pubkey::new_unique();
        let destination_stake_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let lamports = 1_000_000;

        type InstructionFn = fn(&Pubkey, &Pubkey, &Pubkey, u64) -> Instruction;
        let test_vectors: Vec<(InstructionFn, String)> = vec![
            (instruction::move_stake, "moveStake".to_string()),
            (instruction::move_lamports, "moveLamports".to_string()),
        ];

        for (mk_ixn, ixn_string) in test_vectors {
            let instruction = mk_ixn(
                &source_stake_pubkey,
                &destination_stake_pubkey,
                &authorized_pubkey,
                lamports,
            );
            let mut message = Message::new(&[instruction], None);
            assert_eq!(
                parse_stake(
                    &message.instructions[0],
                    &AccountKeys::new(&message.account_keys, None)
                )
                .unwrap(),
                ParsedInstructionEnum {
                    instruction_type: ixn_string,
                    info: json!({
                        "source": source_stake_pubkey.to_string(),
                        "destination": destination_stake_pubkey.to_string(),
                        "stakeAuthority": authorized_pubkey.to_string(),
                        "lamports": lamports,
                    }),
                }
            );
            assert!(
                parse_stake(
                    &message.instructions[0],
                    &AccountKeys::new(&message.account_keys[0..2], None)
                )
                .is_err()
            );
            let keys = message.account_keys.clone();
            message.instructions[0].accounts.pop();
            assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());
        }
    }

    #[test]
    fn test_parse_stake_legacy_layouts() {
        let stake_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let custodian_pubkey = Pubkey::new_unique();
        let other_pubkey = Pubkey::new_unique();
        let authorized = Authorized {
            staker: authorized_pubkey,
            withdrawer: new_authorized_pubkey,
        };
        let seed = "test_seed".to_string();
        let clock = ("clockSysvar", sysvar::clock::ID);
        let rent = ("rentSysvar", sysvar::rent::ID);
        let stake_history = ("stakeHistorySysvar", sysvar::stake_history::ID);
        let stake_config = ("stakeConfigAccount", config::ID);

        for (instruction, index, dropped) in [
            (
                instruction::initialize(&stake_pubkey, &authorized, &Lockup::default()),
                1,
                vec![rent],
            ),
            (
                instruction::authorize(
                    &stake_pubkey,
                    &authorized_pubkey,
                    &new_authorized_pubkey,
                    StakeAuthorize::Staker,
                    None,
                ),
                1,
                vec![clock],
            ),
            (
                instruction::authorize(
                    &stake_pubkey,
                    &authorized_pubkey,
                    &new_authorized_pubkey,
                    StakeAuthorize::Withdrawer,
                    Some(&custodian_pubkey),
                ),
                1,
                vec![clock],
            ),
            (
                instruction::delegate_stake(&stake_pubkey, &authorized_pubkey, &other_pubkey),
                2,
                vec![clock, stake_history, stake_config],
            ),
            (
                instruction::withdraw(&stake_pubkey, &authorized_pubkey, &other_pubkey, 55, None),
                2,
                vec![clock, stake_history],
            ),
            (
                instruction::withdraw(
                    &stake_pubkey,
                    &authorized_pubkey,
                    &other_pubkey,
                    55,
                    Some(&custodian_pubkey),
                ),
                2,
                vec![clock, stake_history],
            ),
            (
                instruction::deactivate_stake(&stake_pubkey, &authorized_pubkey),
                1,
                vec![clock],
            ),
            (
                instruction::merge(&stake_pubkey, &other_pubkey, &authorized_pubkey).remove(0),
                2,
                vec![clock, stake_history],
            ),
            (
                instruction::authorize_with_seed(
                    &stake_pubkey,
                    &authorized_pubkey,
                    seed.clone(),
                    &other_pubkey,
                    &new_authorized_pubkey,
                    StakeAuthorize::Staker,
                    None,
                ),
                2,
                vec![clock],
            ),
            (
                instruction::authorize_with_seed(
                    &stake_pubkey,
                    &authorized_pubkey,
                    seed.clone(),
                    &other_pubkey,
                    &new_authorized_pubkey,
                    StakeAuthorize::Withdrawer,
                    Some(&custodian_pubkey),
                ),
                2,
                vec![clock],
            ),
            (
                instruction::initialize_checked(&stake_pubkey, &authorized),
                1,
                vec![rent],
            ),
            (
                instruction::authorize_checked(
                    &stake_pubkey,
                    &authorized_pubkey,
                    &new_authorized_pubkey,
                    StakeAuthorize::Staker,
                    None,
                ),
                1,
                vec![clock],
            ),
            (
                instruction::authorize_checked(
                    &stake_pubkey,
                    &authorized_pubkey,
                    &new_authorized_pubkey,
                    StakeAuthorize::Withdrawer,
                    Some(&custodian_pubkey),
                ),
                1,
                vec![clock],
            ),
            (
                instruction::authorize_checked_with_seed(
                    &stake_pubkey,
                    &authorized_pubkey,
                    seed.clone(),
                    &other_pubkey,
                    &new_authorized_pubkey,
                    StakeAuthorize::Staker,
                    None,
                ),
                2,
                vec![clock],
            ),
            (
                instruction::authorize_checked_with_seed(
                    &stake_pubkey,
                    &authorized_pubkey,
                    seed.clone(),
                    &other_pubkey,
                    &new_authorized_pubkey,
                    StakeAuthorize::Withdrawer,
                    Some(&custodian_pubkey),
                ),
                2,
                vec![clock],
            ),
        ] {
            let parse = |instruction: &Instruction| {
                let message = Message::new(std::slice::from_ref(instruction), None);
                parse_stake(
                    &message.instructions[0],
                    &AccountKeys::new(&message.account_keys, None),
                )
            };
            let with_dropped = |parsed: Result<ParsedInstructionEnum, ParseInstructionError>| {
                parsed.ok().map(|mut parsed| {
                    for (field, key) in &dropped {
                        parsed.info[*field] = json!(key.to_string());
                    }
                    parsed
                })
            };

            let mut legacy = instruction.clone();
            legacy.accounts.splice(
                index..index,
                dropped
                    .iter()
                    .map(|(_, key)| AccountMeta::new_readonly(*key, false)),
            );

            // The legacy layout parses as the current one plus the dropped accounts
            let expected = with_dropped(parse(&instruction));
            assert!(expected.is_some());
            assert_eq!(parse(&legacy).ok(), expected);

            // and is held to its own account count: losing the account after the
            // dropped ones is judged the same way in both layouts
            if index < instruction.accounts.len() {
                let mut instruction = instruction;
                instruction.accounts.pop();
                legacy.accounts.pop();
                assert_eq!(parse(&legacy).ok(), with_dropped(parse(&instruction)));
            }
        }
    }
}
