use {
    crate::parse_instruction::{
        ParsableProgram, ParseInstructionError, ParsedInstructionEnum, check_num_accounts,
    },
    bincode::deserialize,
    serde_json::{Map, Value, json},
    solana_message::{AccountKeys, compiled_instruction::CompiledInstruction},
    solana_sdk_ids::sysvar,
    solana_stake_interface::instruction::StakeInstruction,
};

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
    // Since its migration to Core BPF, the stake program accepts both the
    // legacy account layouts (which include sysvar and config accounts) and
    // minimal layouts that omit them. The program dispatches between the two
    // by checking whether the pubkey at a given position is the relevant
    // sysvar id, and this parser mirrors that dispatch.
    match stake_instruction {
        StakeInstruction::Initialize(authorized, lockup) => {
            check_num_stake_accounts(&instruction.accounts, 1)?;
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
            let map = value.as_object_mut().unwrap();
            if instruction.accounts.len() >= 2 {
                map.insert(
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
            check_num_stake_accounts(&instruction.accounts, 2)?;
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "newAuthority": new_authorized.to_string(),
                "authorityType": authority_type,
            });
            let map = value.as_object_mut().unwrap();
            if account_keys[instruction.accounts[1] as usize] == sysvar::clock::ID {
                check_num_stake_accounts(&instruction.accounts, 3)?;
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
                map.insert(
                    "authority".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                if instruction.accounts.len() >= 4 {
                    map.insert(
                        "custodian".to_string(),
                        json!(account_keys[instruction.accounts[3] as usize].to_string()),
                    );
                }
            } else {
                map.insert(
                    "authority".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
                if instruction.accounts.len() >= 3 {
                    map.insert(
                        "custodian".to_string(),
                        json!(account_keys[instruction.accounts[2] as usize].to_string()),
                    );
                }
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "authorize".to_string(),
                info: value,
            })
        }
        StakeInstruction::DelegateStake => {
            check_num_stake_accounts(&instruction.accounts, 3)?;
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "voteAccount": account_keys[instruction.accounts[1] as usize].to_string(),
            });
            let map = value.as_object_mut().unwrap();
            if account_keys[instruction.accounts[2] as usize] == sysvar::clock::ID {
                check_num_stake_accounts(&instruction.accounts, 5)?;
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
                if instruction.accounts.len() >= 6 {
                    map.insert(
                        "stakeAuthority".to_string(),
                        json!(account_keys[instruction.accounts[5] as usize].to_string()),
                    );
                }
            } else {
                map.insert(
                    "stakeAuthority".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "delegate".to_string(),
                info: value,
            })
        }
        StakeInstruction::Split(lamports) => {
            check_num_stake_accounts(&instruction.accounts, 2)?;
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "newSplitAccount": account_keys[instruction.accounts[1] as usize].to_string(),
                "lamports": lamports,
            });
            let map = value.as_object_mut().unwrap();
            if instruction.accounts.len() >= 3 {
                map.insert(
                    "stakeAuthority".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "split".to_string(),
                info: value,
            })
        }
        StakeInstruction::Withdraw(lamports) => {
            check_num_stake_accounts(&instruction.accounts, 3)?;
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "destination": account_keys[instruction.accounts[1] as usize].to_string(),
                "lamports": lamports,
            });
            let map = value.as_object_mut().unwrap();
            if account_keys[instruction.accounts[2] as usize] == sysvar::clock::ID {
                check_num_stake_accounts(&instruction.accounts, 5)?;
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                map.insert(
                    "stakeHistorySysvar".to_string(),
                    json!(account_keys[instruction.accounts[3] as usize].to_string()),
                );
                map.insert(
                    "withdrawAuthority".to_string(),
                    json!(account_keys[instruction.accounts[4] as usize].to_string()),
                );
                if instruction.accounts.len() >= 6 {
                    map.insert(
                        "custodian".to_string(),
                        json!(account_keys[instruction.accounts[5] as usize].to_string()),
                    );
                }
            } else {
                map.insert(
                    "withdrawAuthority".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                if instruction.accounts.len() >= 4 {
                    map.insert(
                        "custodian".to_string(),
                        json!(account_keys[instruction.accounts[3] as usize].to_string()),
                    );
                }
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "withdraw".to_string(),
                info: value,
            })
        }
        StakeInstruction::Deactivate => {
            check_num_stake_accounts(&instruction.accounts, 2)?;
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
            });
            let map = value.as_object_mut().unwrap();
            if account_keys[instruction.accounts[1] as usize] == sysvar::clock::ID {
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
                if instruction.accounts.len() >= 3 {
                    map.insert(
                        "stakeAuthority".to_string(),
                        json!(account_keys[instruction.accounts[2] as usize].to_string()),
                    );
                }
            } else {
                map.insert(
                    "stakeAuthority".to_string(),
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
            check_num_stake_accounts(&instruction.accounts, 3)?;
            let mut value = json!({
                "destination": account_keys[instruction.accounts[0] as usize].to_string(),
                "source": account_keys[instruction.accounts[1] as usize].to_string(),
            });
            let map = value.as_object_mut().unwrap();
            if account_keys[instruction.accounts[2] as usize] == sysvar::clock::ID {
                check_num_stake_accounts(&instruction.accounts, 4)?;
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                map.insert(
                    "stakeHistorySysvar".to_string(),
                    json!(account_keys[instruction.accounts[3] as usize].to_string()),
                );
                if instruction.accounts.len() >= 5 {
                    map.insert(
                        "stakeAuthority".to_string(),
                        json!(account_keys[instruction.accounts[4] as usize].to_string()),
                    );
                }
            } else {
                map.insert(
                    "stakeAuthority".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
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
            if instruction.accounts.len() >= 3 {
                if account_keys[instruction.accounts[2] as usize] == sysvar::clock::ID {
                    map.insert(
                        "clockSysvar".to_string(),
                        json!(account_keys[instruction.accounts[2] as usize].to_string()),
                    );
                    if instruction.accounts.len() >= 4 {
                        map.insert(
                            "custodian".to_string(),
                            json!(account_keys[instruction.accounts[3] as usize].to_string()),
                        );
                    }
                } else {
                    map.insert(
                        "custodian".to_string(),
                        json!(account_keys[instruction.accounts[2] as usize].to_string()),
                    );
                }
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "authorizeWithSeed".to_string(),
                info: value,
            })
        }
        StakeInstruction::InitializeChecked => {
            check_num_stake_accounts(&instruction.accounts, 3)?;
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
            });
            let map = value.as_object_mut().unwrap();
            if account_keys[instruction.accounts[1] as usize] == sysvar::rent::ID {
                check_num_stake_accounts(&instruction.accounts, 4)?;
                map.insert(
                    "rentSysvar".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
                map.insert(
                    "staker".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                map.insert(
                    "withdrawer".to_string(),
                    json!(account_keys[instruction.accounts[3] as usize].to_string()),
                );
            } else {
                map.insert(
                    "staker".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
                map.insert(
                    "withdrawer".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "initializeChecked".to_string(),
                info: value,
            })
        }
        StakeInstruction::AuthorizeChecked(authority_type) => {
            check_num_stake_accounts(&instruction.accounts, 3)?;
            let mut value = json!({
                "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                "authorityType": authority_type,
            });
            let map = value.as_object_mut().unwrap();
            if account_keys[instruction.accounts[1] as usize] == sysvar::clock::ID {
                check_num_stake_accounts(&instruction.accounts, 4)?;
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
                map.insert(
                    "authority".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                map.insert(
                    "newAuthority".to_string(),
                    json!(account_keys[instruction.accounts[3] as usize].to_string()),
                );
                if instruction.accounts.len() >= 5 {
                    map.insert(
                        "custodian".to_string(),
                        json!(account_keys[instruction.accounts[4] as usize].to_string()),
                    );
                }
            } else {
                map.insert(
                    "authority".to_string(),
                    json!(account_keys[instruction.accounts[1] as usize].to_string()),
                );
                map.insert(
                    "newAuthority".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                if instruction.accounts.len() >= 4 {
                    map.insert(
                        "custodian".to_string(),
                        json!(account_keys[instruction.accounts[3] as usize].to_string()),
                    );
                }
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "authorizeChecked".to_string(),
                info: value,
            })
        }
        StakeInstruction::AuthorizeCheckedWithSeed(args) => {
            check_num_stake_accounts(&instruction.accounts, 3)?;
            let mut value = json!({
                    "stakeAccount": account_keys[instruction.accounts[0] as usize].to_string(),
                    "authorityBase": account_keys[instruction.accounts[1] as usize].to_string(),
                    "authorityType": args.stake_authorize,
                    "authoritySeed": args.authority_seed,
                    "authorityOwner": args.authority_owner.to_string(),
            });
            let map = value.as_object_mut().unwrap();
            if account_keys[instruction.accounts[2] as usize] == sysvar::clock::ID {
                check_num_stake_accounts(&instruction.accounts, 4)?;
                map.insert(
                    "clockSysvar".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                map.insert(
                    "newAuthorized".to_string(),
                    json!(account_keys[instruction.accounts[3] as usize].to_string()),
                );
                if instruction.accounts.len() >= 5 {
                    map.insert(
                        "custodian".to_string(),
                        json!(account_keys[instruction.accounts[4] as usize].to_string()),
                    );
                }
            } else {
                map.insert(
                    "newAuthorized".to_string(),
                    json!(account_keys[instruction.accounts[2] as usize].to_string()),
                );
                if instruction.accounts.len() >= 4 {
                    map.insert(
                        "custodian".to_string(),
                        json!(account_keys[instruction.accounts[3] as usize].to_string()),
                    );
                }
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
        solana_sdk_ids::sysvar,
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
                    "rentSysvar": sysvar::rent::ID.to_string(),
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
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without the rent sysvar
        let instruction = Instruction::new_with_bincode(
            solana_sdk_ids::stake::ID,
            &StakeInstruction::Initialize(authorized, lockup),
            vec![AccountMeta::new(stake_pubkey, false)],
        );
        let message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
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
                &AccountKeys::new(&message.account_keys[0..2], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without the clock sysvar, with and without custodian
        for custodian in [None, Some(custodian_pubkey)] {
            let mut account_metas = vec![
                AccountMeta::new(stake_pubkey, false),
                AccountMeta::new_readonly(authorized_pubkey, true),
            ];
            let mut expected_info = json!({
                "stakeAccount": stake_pubkey.to_string(),
                "authority": authorized_pubkey.to_string(),
                "newAuthority": new_authorized_pubkey.to_string(),
                "authorityType": StakeAuthorize::Staker,
            });
            if let Some(custodian) = custodian {
                account_metas.push(AccountMeta::new_readonly(custodian, true));
                expected_info["custodian"] = json!(custodian.to_string());
            }
            let instruction = Instruction::new_with_bincode(
                solana_sdk_ids::stake::ID,
                &StakeInstruction::Authorize(new_authorized_pubkey, StakeAuthorize::Staker),
                account_metas,
            );
            let message = Message::new(&[instruction], None);
            assert_eq!(
                parse_stake(
                    &message.instructions[0],
                    &AccountKeys::new(&message.account_keys, None)
                )
                .unwrap(),
                ParsedInstructionEnum {
                    instruction_type: "authorize".to_string(),
                    info: expected_info,
                }
            );
        }
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "stakeHistorySysvar": sysvar::stake_history::ID.to_string(),
                    "stakeConfigAccount": config::ID.to_string(),
                    "stakeAuthority": authorized_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..5], None)
            )
            .is_err()
        );
        // The stake authority is optional in the legacy layout
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert_eq!(
            parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).unwrap(),
            ParsedInstructionEnum {
                instruction_type: "delegate".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "voteAccount": vote_pubkey.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "stakeHistorySysvar": sysvar::stake_history::ID.to_string(),
                    "stakeConfigAccount": config::ID.to_string(),
                }),
            }
        );
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without sysvar or config accounts
        let instruction = Instruction::new_with_bincode(
            solana_sdk_ids::stake::ID,
            &StakeInstruction::DelegateStake,
            vec![
                AccountMeta::new(stake_pubkey, false),
                AccountMeta::new_readonly(vote_pubkey, false),
                AccountMeta::new_readonly(authorized_pubkey, true),
            ],
        );
        let message = Message::new(&[instruction], None);
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

        // Minimal layout without the stake authority
        let instruction = Instruction::new_with_bincode(
            solana_sdk_ids::stake::ID,
            &StakeInstruction::Split(lamports),
            vec![
                AccountMeta::new(stake_pubkey, false),
                AccountMeta::new(split_stake_pubkey, false),
            ],
        );
        let message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "split".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "newSplitAccount": split_stake_pubkey.to_string(),
                    "lamports": lamports,
                }),
            }
        );
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "stakeHistorySysvar": sysvar::stake_history::ID.to_string(),
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "stakeHistorySysvar": sysvar::stake_history::ID.to_string(),
                    "withdrawAuthority": withdrawer_pubkey.to_string(),
                    "custodian": custodian_pubkey.to_string(),
                    "lamports": lamports,
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..4], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without sysvar accounts, with and without custodian
        for custodian in [None, Some(custodian_pubkey)] {
            let mut account_metas = vec![
                AccountMeta::new(stake_pubkey, false),
                AccountMeta::new(to_pubkey, false),
                AccountMeta::new_readonly(withdrawer_pubkey, true),
            ];
            let mut expected_info = json!({
                "stakeAccount": stake_pubkey.to_string(),
                "destination": to_pubkey.to_string(),
                "withdrawAuthority": withdrawer_pubkey.to_string(),
                "lamports": lamports,
            });
            if let Some(custodian) = custodian {
                account_metas.push(AccountMeta::new_readonly(custodian, true));
                expected_info["custodian"] = json!(custodian.to_string());
            }
            let instruction = Instruction::new_with_bincode(
                solana_sdk_ids::stake::ID,
                &StakeInstruction::Withdraw(lamports),
                account_metas,
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
                    info: expected_info,
                }
            );
        }
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
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
        // The stake authority is optional in the legacy layout
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert_eq!(
            parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).unwrap(),
            ParsedInstructionEnum {
                instruction_type: "deactivate".to_string(),
                info: json!({
                    "stakeAccount": stake_pubkey.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                }),
            }
        );
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without the clock sysvar
        let instruction = Instruction::new_with_bincode(
            solana_sdk_ids::stake::ID,
            &StakeInstruction::Deactivate,
            vec![
                AccountMeta::new(stake_pubkey, false),
                AccountMeta::new_readonly(authorized_pubkey, true),
            ],
        );
        let message = Message::new(&[instruction], None);
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "stakeHistorySysvar": sysvar::stake_history::ID.to_string(),
                    "stakeAuthority": authorized_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..4], None)
            )
            .is_err()
        );
        // The stake authority is optional in the legacy layout
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert_eq!(
            parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).unwrap(),
            ParsedInstructionEnum {
                instruction_type: "merge".to_string(),
                info: json!({
                    "destination": destination_stake_pubkey.to_string(),
                    "source": source_stake_pubkey.to_string(),
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "stakeHistorySysvar": sysvar::stake_history::ID.to_string(),
                }),
            }
        );
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without sysvar accounts
        let instruction = Instruction::new_with_bincode(
            solana_sdk_ids::stake::ID,
            &StakeInstruction::Merge,
            vec![
                AccountMeta::new(destination_stake_pubkey, false),
                AccountMeta::new(source_stake_pubkey, false),
                AccountMeta::new_readonly(authorized_pubkey, true),
            ],
        );
        let message = Message::new(&[instruction], None);
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
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
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without the clock sysvar; the custodian follows the
        // authority base directly
        let args = instruction::AuthorizeWithSeedArgs {
            new_authorized_pubkey,
            stake_authorize: StakeAuthorize::Withdrawer,
            authority_seed: seed.to_string(),
            authority_owner: authority_owner_pubkey,
        };
        let instruction = Instruction::new_with_bincode(
            solana_sdk_ids::stake::ID,
            &StakeInstruction::AuthorizeWithSeed(args),
            vec![
                AccountMeta::new(stake_pubkey, false),
                AccountMeta::new_readonly(authority_base_pubkey, true),
                AccountMeta::new_readonly(custodian_pubkey, true),
            ],
        );
        let message = Message::new(&[instruction], None);
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
                    "rentSysvar": sysvar::rent::ID.to_string(),
                    "staker": authorized.staker.to_string(),
                    "withdrawer": authorized.withdrawer.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[1],
                &AccountKeys::new(&message.account_keys[0..3], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without the rent sysvar
        let instruction = Instruction::new_with_bincode(
            solana_sdk_ids::stake::ID,
            &StakeInstruction::InitializeChecked,
            vec![
                AccountMeta::new(stake_pubkey, false),
                AccountMeta::new_readonly(authorized.staker, false),
                AccountMeta::new_readonly(authorized.withdrawer, true),
            ],
        );
        let message = Message::new(&[instruction], None);
        assert_eq!(
            parse_stake(
                &message.instructions[0],
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "authority": authorized_pubkey.to_string(),
                    "newAuthority": new_authorized_pubkey.to_string(),
                    "authorityType": StakeAuthorize::Staker,
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
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
                &AccountKeys::new(&message.account_keys[0..4], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without the clock sysvar, with and without custodian
        for custodian in [None, Some(custodian_pubkey)] {
            let mut account_metas = vec![
                AccountMeta::new(stake_pubkey, false),
                AccountMeta::new_readonly(authorized_pubkey, true),
                AccountMeta::new_readonly(new_authorized_pubkey, true),
            ];
            let mut expected_info = json!({
                "stakeAccount": stake_pubkey.to_string(),
                "authority": authorized_pubkey.to_string(),
                "newAuthority": new_authorized_pubkey.to_string(),
                "authorityType": StakeAuthorize::Staker,
            });
            if let Some(custodian) = custodian {
                account_metas.push(AccountMeta::new_readonly(custodian, true));
                expected_info["custodian"] = json!(custodian.to_string());
            }
            let instruction = Instruction::new_with_bincode(
                solana_sdk_ids::stake::ID,
                &StakeInstruction::AuthorizeChecked(StakeAuthorize::Staker),
                account_metas,
            );
            let message = Message::new(&[instruction], None);
            assert_eq!(
                parse_stake(
                    &message.instructions[0],
                    &AccountKeys::new(&message.account_keys, None)
                )
                .unwrap(),
                ParsedInstructionEnum {
                    instruction_type: "authorizeChecked".to_string(),
                    info: expected_info,
                }
            );
        }
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
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
                    "clockSysvar": sysvar::clock::ID.to_string(),
                    "custodian": custodian_pubkey.to_string(),
                }),
            }
        );
        assert!(
            parse_stake(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys[0..4], None)
            )
            .is_err()
        );
        let keys = message.account_keys.clone();
        message.instructions[0].accounts.pop();
        message.instructions[0].accounts.pop();
        assert!(parse_stake(&message.instructions[0], &AccountKeys::new(&keys, None)).is_err());

        // Minimal layout without the clock sysvar, with and without custodian
        for custodian in [None, Some(custodian_pubkey)] {
            let mut account_metas = vec![
                AccountMeta::new(stake_pubkey, false),
                AccountMeta::new_readonly(authority_base_pubkey, true),
                AccountMeta::new_readonly(new_authorized_pubkey, true),
            ];
            let mut expected_info = json!({
                "stakeAccount": stake_pubkey.to_string(),
                "authorityOwner": authority_owner_pubkey.to_string(),
                "newAuthorized": new_authorized_pubkey.to_string(),
                "authorityBase": authority_base_pubkey.to_string(),
                "authoritySeed": seed,
                "authorityType": StakeAuthorize::Staker,
            });
            if let Some(custodian) = custodian {
                account_metas.push(AccountMeta::new_readonly(custodian, true));
                expected_info["custodian"] = json!(custodian.to_string());
            }
            let args = instruction::AuthorizeCheckedWithSeedArgs {
                stake_authorize: StakeAuthorize::Staker,
                authority_seed: seed.to_string(),
                authority_owner: authority_owner_pubkey,
            };
            let instruction = Instruction::new_with_bincode(
                solana_sdk_ids::stake::ID,
                &StakeInstruction::AuthorizeCheckedWithSeed(args),
                account_metas,
            );
            let message = Message::new(&[instruction], None);
            assert_eq!(
                parse_stake(
                    &message.instructions[0],
                    &AccountKeys::new(&message.account_keys, None)
                )
                .unwrap(),
                ParsedInstructionEnum {
                    instruction_type: "authorizeCheckedWithSeed".to_string(),
                    info: expected_info,
                }
            );
        }
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
}
