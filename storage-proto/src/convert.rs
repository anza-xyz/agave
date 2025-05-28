use {
    crate::{StoredExtendedRewards, StoredTransactionError, StoredTransactionStatusMeta},
    solana_account_decoder::parse_token::{real_number_string_trimmed, UiTokenAmount},
    solana_hash::{Hash, HASH_BYTES},
    solana_instruction::error::InstructionError,
    solana_message::{
        compiled_instruction::CompiledInstruction,
        legacy::Message as LegacyMessage,
        v0::{self, LoadedAddresses, MessageAddressTableLookup},
        MessageHeader, VersionedMessage,
    },
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
    solana_transaction_context::TransactionReturnData,
    solana_transaction_error::TransactionError,
    solana_transaction_status::{
        ConfirmedBlock, EntrySummary, InnerInstruction, InnerInstructions, Reward, RewardType,
        RewardsAndNumPartitions, TransactionByAddrInfo, TransactionStatusMeta,
        TransactionTokenBalance, TransactionWithStatusMeta, VersionedConfirmedBlock,
        VersionedTransactionWithStatusMeta,
    },
    std::{
        convert::{TryFrom, TryInto},
        str::FromStr,
    },
};

pub mod generated {
    include!(concat!(
        env!("OUT_DIR"),
        "/solana.storage.confirmed_block.rs"
    ));
}

pub mod tx_by_addr {
    include!(concat!(
        env!("OUT_DIR"),
        "/solana.storage.transaction_by_addr.rs"
    ));
}

pub mod entries {
    include!(concat!(env!("OUT_DIR"), "/solana.storage.entries.rs"));
}

impl From<Vec<Reward>> for generated::Rewards {
    fn from(rewards: Vec<Reward>) -> Self {
        Self {
            rewards: rewards.into_iter().map(|r| r.into()).collect(),
            num_partitions: None,
        }
    }
}

impl From<RewardsAndNumPartitions> for generated::Rewards {
    fn from(input: RewardsAndNumPartitions) -> Self {
        Self {
            rewards: input.rewards.into_iter().map(|r| r.into()).collect(),
            num_partitions: input.num_partitions.map(|n| n.into()),
        }
    }
}

impl From<generated::Rewards> for Vec<Reward> {
    fn from(rewards: generated::Rewards) -> Self {
        rewards.rewards.into_iter().map(|r| r.into()).collect()
    }
}

impl From<generated::Rewards> for (Vec<Reward>, Option<u64>) {
    fn from(rewards: generated::Rewards) -> Self {
        (
            rewards.rewards.into_iter().map(|r| r.into()).collect(),
            rewards
                .num_partitions
                .map(|generated::NumPartitions { num_partitions }| num_partitions),
        )
    }
}

impl From<StoredExtendedRewards> for generated::Rewards {
    fn from(rewards: StoredExtendedRewards) -> Self {
        Self {
            rewards: rewards
                .into_iter()
                .map(|r| {
                    let r: Reward = r.into();
                    r.into()
                })
                .collect(),
            num_partitions: None,
        }
    }
}

impl From<generated::Rewards> for StoredExtendedRewards {
    fn from(rewards: generated::Rewards) -> Self {
        rewards
            .rewards
            .into_iter()
            .map(|r| {
                let r: Reward = r.into();
                r.into()
            })
            .collect()
    }
}

impl From<Reward> for generated::Reward {
    fn from(reward: Reward) -> Self {
        Self {
            pubkey: reward.pubkey,
            lamports: reward.lamports,
            post_balance: reward.post_balance,
            reward_type: match reward.reward_type {
                None => generated::RewardType::Unspecified,
                Some(RewardType::Fee) => generated::RewardType::Fee,
                Some(RewardType::Rent) => generated::RewardType::Rent,
                Some(RewardType::Staking) => generated::RewardType::Staking,
                Some(RewardType::Voting) => generated::RewardType::Voting,
            } as i32,
            commission: reward.commission.map(|c| c.to_string()).unwrap_or_default(),
        }
    }
}

impl From<generated::Reward> for Reward {
    fn from(reward: generated::Reward) -> Self {
        Self {
            pubkey: reward.pubkey,
            lamports: reward.lamports,
            post_balance: reward.post_balance,
            reward_type: match reward.reward_type {
                0 => None,
                1 => Some(RewardType::Fee),
                2 => Some(RewardType::Rent),
                3 => Some(RewardType::Staking),
                4 => Some(RewardType::Voting),
                _ => None,
            },
            commission: reward.commission.parse::<u8>().ok(),
        }
    }
}

impl From<u64> for generated::NumPartitions {
    fn from(num_partitions: u64) -> Self {
        Self { num_partitions }
    }
}

impl From<VersionedConfirmedBlock> for generated::ConfirmedBlock {
    fn from(confirmed_block: VersionedConfirmedBlock) -> Self {
        let VersionedConfirmedBlock {
            previous_blockhash,
            blockhash,
            parent_slot,
            transactions,
            rewards,
            num_partitions,
            block_time,
            block_height,
        } = confirmed_block;

        Self {
            previous_blockhash,
            blockhash,
            parent_slot,
            transactions: transactions.into_iter().map(|tx| tx.into()).collect(),
            rewards: rewards.into_iter().map(|r| r.into()).collect(),
            num_partitions: num_partitions.map(Into::into),
            block_time: block_time.map(|timestamp| generated::UnixTimestamp { timestamp }),
            block_height: block_height.map(|block_height| generated::BlockHeight { block_height }),
        }
    }
}

impl TryFrom<generated::ConfirmedBlock> for ConfirmedBlock {
    type Error = bincode::Error;
    fn try_from(
        confirmed_block: generated::ConfirmedBlock,
    ) -> std::result::Result<Self, Self::Error> {
        let generated::ConfirmedBlock {
            previous_blockhash,
            blockhash,
            parent_slot,
            transactions,
            rewards,
            num_partitions,
            block_time,
            block_height,
        } = confirmed_block;

        Ok(Self {
            previous_blockhash,
            blockhash,
            parent_slot,
            transactions: transactions
                .into_iter()
                .map(|tx| tx.try_into())
                .collect::<std::result::Result<Vec<_>, Self::Error>>()?,
            rewards: rewards.into_iter().map(|r| r.into()).collect(),
            num_partitions: num_partitions
                .map(|generated::NumPartitions { num_partitions }| num_partitions),
            block_time: block_time.map(|generated::UnixTimestamp { timestamp }| timestamp),
            block_height: block_height.map(|generated::BlockHeight { block_height }| block_height),
        })
    }
}

impl From<TransactionWithStatusMeta> for generated::ConfirmedTransaction {
    fn from(tx_with_meta: TransactionWithStatusMeta) -> Self {
        match tx_with_meta {
            TransactionWithStatusMeta::MissingMetadata(transaction) => Self {
                transaction: Some(generated::Transaction::from(transaction)),
                meta: None,
            },
            TransactionWithStatusMeta::Complete(tx_with_meta) => Self::from(tx_with_meta),
        }
    }
}

impl From<VersionedTransactionWithStatusMeta> for generated::ConfirmedTransaction {
    fn from(value: VersionedTransactionWithStatusMeta) -> Self {
        Self {
            transaction: Some(value.transaction.into()),
            meta: Some(value.meta.into()),
        }
    }
}

impl TryFrom<generated::ConfirmedTransaction> for TransactionWithStatusMeta {
    type Error = bincode::Error;
    fn try_from(value: generated::ConfirmedTransaction) -> std::result::Result<Self, Self::Error> {
        let meta = value.meta.map(|meta| meta.try_into()).transpose()?;
        let transaction = value.transaction.expect("transaction is required").into();
        Ok(match meta {
            Some(meta) => Self::Complete(VersionedTransactionWithStatusMeta { transaction, meta }),
            None => Self::MissingMetadata(
                transaction
                    .into_legacy_transaction()
                    .expect("meta is required for versioned transactions"),
            ),
        })
    }
}

impl From<Transaction> for generated::Transaction {
    fn from(value: Transaction) -> Self {
        Self {
            signatures: value
                .signatures
                .into_iter()
                .map(|signature| <Signature as AsRef<[u8]>>::as_ref(&signature).into())
                .collect(),
            message: Some(value.message.into()),
        }
    }
}

impl From<VersionedTransaction> for generated::Transaction {
    fn from(value: VersionedTransaction) -> Self {
        Self {
            signatures: value
                .signatures
                .into_iter()
                .map(|signature| <Signature as AsRef<[u8]>>::as_ref(&signature).into())
                .collect(),
            message: Some(value.message.into()),
        }
    }
}

impl From<generated::Transaction> for VersionedTransaction {
    fn from(value: generated::Transaction) -> Self {
        Self {
            signatures: value
                .signatures
                .into_iter()
                .map(Signature::try_from)
                .collect::<Result<_, _>>()
                .unwrap(),
            message: value.message.expect("message is required").into(),
        }
    }
}

impl From<TransactionError> for generated::TransactionError {
    fn from(value: TransactionError) -> Self {
        let stored_error = Into::<StoredTransactionError>::into(value).0;
        Self { err: stored_error }
    }
}

impl From<generated::TransactionError> for TransactionError {
    fn from(value: generated::TransactionError) -> Self {
        let stored_error = StoredTransactionError(value.err);
        stored_error.into()
    }
}

impl From<LegacyMessage> for generated::Message {
    fn from(message: LegacyMessage) -> Self {
        Self {
            header: Some(message.header.into()),
            account_keys: message
                .account_keys
                .iter()
                .map(|key| <Pubkey as AsRef<[u8]>>::as_ref(key).into())
                .collect(),
            recent_blockhash: message.recent_blockhash.to_bytes().into(),
            instructions: message
                .instructions
                .into_iter()
                .map(|ix| ix.into())
                .collect(),
            versioned: false,
            address_table_lookups: vec![],
        }
    }
}

impl From<VersionedMessage> for generated::Message {
    fn from(message: VersionedMessage) -> Self {
        match message {
            VersionedMessage::Legacy(message) => Self::from(message),
            VersionedMessage::V0(message) => Self {
                header: Some(message.header.into()),
                account_keys: message
                    .account_keys
                    .iter()
                    .map(|key| <Pubkey as AsRef<[u8]>>::as_ref(key).into())
                    .collect(),
                recent_blockhash: message.recent_blockhash.to_bytes().into(),
                instructions: message
                    .instructions
                    .into_iter()
                    .map(|ix| ix.into())
                    .collect(),
                versioned: true,
                address_table_lookups: message
                    .address_table_lookups
                    .into_iter()
                    .map(|lookup| lookup.into())
                    .collect(),
            },
        }
    }
}

impl From<generated::Message> for VersionedMessage {
    fn from(value: generated::Message) -> Self {
        let header = value.header.expect("header is required").into();
        let account_keys = value
            .account_keys
            .into_iter()
            .map(|key| Pubkey::try_from(key).unwrap())
            .collect();
        let recent_blockhash = <[u8; HASH_BYTES]>::try_from(value.recent_blockhash)
            .map(Hash::new_from_array)
            .unwrap();
        let instructions = value.instructions.into_iter().map(|ix| ix.into()).collect();
        let address_table_lookups = value
            .address_table_lookups
            .into_iter()
            .map(|lookup| lookup.into())
            .collect();

        if !value.versioned {
            Self::Legacy(LegacyMessage {
                header,
                account_keys,
                recent_blockhash,
                instructions,
            })
        } else {
            Self::V0(v0::Message {
                header,
                account_keys,
                recent_blockhash,
                instructions,
                address_table_lookups,
            })
        }
    }
}

impl From<MessageHeader> for generated::MessageHeader {
    fn from(value: MessageHeader) -> Self {
        Self {
            num_required_signatures: value.num_required_signatures as u32,
            num_readonly_signed_accounts: value.num_readonly_signed_accounts as u32,
            num_readonly_unsigned_accounts: value.num_readonly_unsigned_accounts as u32,
        }
    }
}

impl From<generated::MessageHeader> for MessageHeader {
    fn from(value: generated::MessageHeader) -> Self {
        Self {
            num_required_signatures: value.num_required_signatures as u8,
            num_readonly_signed_accounts: value.num_readonly_signed_accounts as u8,
            num_readonly_unsigned_accounts: value.num_readonly_unsigned_accounts as u8,
        }
    }
}

impl From<TransactionStatusMeta> for generated::TransactionStatusMeta {
    fn from(value: TransactionStatusMeta) -> Self {
        let TransactionStatusMeta {
            status,
            fee,
            pre_balances,
            post_balances,
            inner_instructions,
            log_messages,
            pre_token_balances,
            post_token_balances,
            rewards,
            loaded_addresses,
            return_data,
            compute_units_consumed,
            cost_units,
        } = value;
        let err = status.err().map(Into::into);
        let inner_instructions_none = inner_instructions.is_none();
        let inner_instructions = inner_instructions
            .unwrap_or_default()
            .into_iter()
            .map(|ii| ii.into())
            .collect();
        let log_messages_none = log_messages.is_none();
        let log_messages = log_messages.unwrap_or_default();
        let pre_token_balances = pre_token_balances
            .unwrap_or_default()
            .into_iter()
            .map(|balance| balance.into())
            .collect();
        let post_token_balances = post_token_balances
            .unwrap_or_default()
            .into_iter()
            .map(|balance| balance.into())
            .collect();
        let rewards = rewards
            .unwrap_or_default()
            .into_iter()
            .map(|reward| reward.into())
            .collect();
        let loaded_writable_addresses = loaded_addresses
            .writable
            .into_iter()
            .map(|key| <Pubkey as AsRef<[u8]>>::as_ref(&key).into())
            .collect();
        let loaded_readonly_addresses = loaded_addresses
            .readonly
            .into_iter()
            .map(|key| <Pubkey as AsRef<[u8]>>::as_ref(&key).into())
            .collect();
        let return_data_none = return_data.is_none();
        let return_data = return_data.map(|return_data| return_data.into());

        Self {
            err,
            fee,
            pre_balances,
            post_balances,
            inner_instructions,
            inner_instructions_none,
            log_messages,
            log_messages_none,
            pre_token_balances,
            post_token_balances,
            rewards,
            loaded_writable_addresses,
            loaded_readonly_addresses,
            return_data,
            return_data_none,
            compute_units_consumed,
            cost_units,
        }
    }
}

impl From<StoredTransactionStatusMeta> for generated::TransactionStatusMeta {
    fn from(meta: StoredTransactionStatusMeta) -> Self {
        let meta: TransactionStatusMeta = meta.into();
        meta.into()
    }
}

impl TryFrom<generated::TransactionStatusMeta> for TransactionStatusMeta {
    type Error = bincode::Error;

    fn try_from(value: generated::TransactionStatusMeta) -> std::result::Result<Self, Self::Error> {
        let generated::TransactionStatusMeta {
            err,
            fee,
            pre_balances,
            post_balances,
            inner_instructions,
            inner_instructions_none,
            log_messages,
            log_messages_none,
            pre_token_balances,
            post_token_balances,
            rewards,
            loaded_writable_addresses,
            loaded_readonly_addresses,
            return_data,
            return_data_none,
            compute_units_consumed,
            cost_units,
        } = value;
        let status = match err {
            None => Ok(()),
            Some(tx_error) => Err(tx_error.into()),
        };
        let inner_instructions = if inner_instructions_none {
            None
        } else {
            Some(
                inner_instructions
                    .into_iter()
                    .map(|inner| inner.into())
                    .collect(),
            )
        };
        let log_messages = if log_messages_none {
            None
        } else {
            Some(log_messages)
        };
        let pre_token_balances = Some(
            pre_token_balances
                .into_iter()
                .map(|balance| balance.into())
                .collect(),
        );
        let post_token_balances = Some(
            post_token_balances
                .into_iter()
                .map(|balance| balance.into())
                .collect(),
        );
        let rewards = Some(rewards.into_iter().map(|reward| reward.into()).collect());
        let loaded_addresses = LoadedAddresses {
            writable: loaded_writable_addresses
                .into_iter()
                .map(Pubkey::try_from)
                .collect::<Result<_, _>>()
                .map_err(|err| {
                    let err = format!("Invalid writable address: {err:?}");
                    Self::Error::new(bincode::ErrorKind::Custom(err))
                })?,
            readonly: loaded_readonly_addresses
                .into_iter()
                .map(Pubkey::try_from)
                .collect::<Result<_, _>>()
                .map_err(|err| {
                    let err = format!("Invalid readonly address: {err:?}");
                    Self::Error::new(bincode::ErrorKind::Custom(err))
                })?,
        };
        let return_data = if return_data_none {
            None
        } else {
            return_data.map(|return_data| return_data.into())
        };
        Ok(Self {
            status,
            fee,
            pre_balances,
            post_balances,
            inner_instructions,
            log_messages,
            pre_token_balances,
            post_token_balances,
            rewards,
            loaded_addresses,
            return_data,
            compute_units_consumed,
            cost_units,
        })
    }
}

impl From<InnerInstructions> for generated::InnerInstructions {
    fn from(value: InnerInstructions) -> Self {
        Self {
            index: value.index as u32,
            instructions: value.instructions.into_iter().map(|i| i.into()).collect(),
        }
    }
}

impl From<generated::InnerInstructions> for InnerInstructions {
    fn from(value: generated::InnerInstructions) -> Self {
        Self {
            index: value.index as u8,
            instructions: value.instructions.into_iter().map(|i| i.into()).collect(),
        }
    }
}

impl From<TransactionTokenBalance> for generated::TokenBalance {
    fn from(value: TransactionTokenBalance) -> Self {
        Self {
            account_index: value.account_index as u32,
            mint: value.mint,
            ui_token_amount: Some(generated::UiTokenAmount {
                ui_amount: value.ui_token_amount.ui_amount.unwrap_or_default(),
                decimals: value.ui_token_amount.decimals as u32,
                amount: value.ui_token_amount.amount,
                ui_amount_string: value.ui_token_amount.ui_amount_string,
            }),
            owner: value.owner,
            program_id: value.program_id,
        }
    }
}

impl From<generated::TokenBalance> for TransactionTokenBalance {
    fn from(value: generated::TokenBalance) -> Self {
        let ui_token_amount = value.ui_token_amount.unwrap_or_default();
        Self {
            account_index: value.account_index as u8,
            mint: value.mint,
            ui_token_amount: UiTokenAmount {
                ui_amount: if (ui_token_amount.ui_amount - f64::default()).abs() > f64::EPSILON {
                    Some(ui_token_amount.ui_amount)
                } else {
                    None
                },
                decimals: ui_token_amount.decimals as u8,
                amount: ui_token_amount.amount.clone(),
                ui_amount_string: if !ui_token_amount.ui_amount_string.is_empty() {
                    ui_token_amount.ui_amount_string
                } else {
                    real_number_string_trimmed(
                        u64::from_str(&ui_token_amount.amount).unwrap_or_default(),
                        ui_token_amount.decimals as u8,
                    )
                },
            },
            owner: value.owner,
            program_id: value.program_id,
        }
    }
}

impl From<MessageAddressTableLookup> for generated::MessageAddressTableLookup {
    fn from(lookup: MessageAddressTableLookup) -> Self {
        Self {
            account_key: <Pubkey as AsRef<[u8]>>::as_ref(&lookup.account_key).into(),
            writable_indexes: lookup.writable_indexes,
            readonly_indexes: lookup.readonly_indexes,
        }
    }
}

impl From<generated::MessageAddressTableLookup> for MessageAddressTableLookup {
    fn from(value: generated::MessageAddressTableLookup) -> Self {
        Self {
            account_key: Pubkey::try_from(value.account_key).unwrap(),
            writable_indexes: value.writable_indexes,
            readonly_indexes: value.readonly_indexes,
        }
    }
}

impl From<TransactionReturnData> for generated::ReturnData {
    fn from(value: TransactionReturnData) -> Self {
        Self {
            program_id: <Pubkey as AsRef<[u8]>>::as_ref(&value.program_id).into(),
            data: value.data,
        }
    }
}

impl From<generated::ReturnData> for TransactionReturnData {
    fn from(value: generated::ReturnData) -> Self {
        Self {
            program_id: Pubkey::try_from(value.program_id).unwrap(),
            data: value.data,
        }
    }
}

impl From<CompiledInstruction> for generated::CompiledInstruction {
    fn from(value: CompiledInstruction) -> Self {
        Self {
            program_id_index: value.program_id_index as u32,
            accounts: value.accounts,
            data: value.data,
        }
    }
}

impl From<generated::CompiledInstruction> for CompiledInstruction {
    fn from(value: generated::CompiledInstruction) -> Self {
        Self {
            program_id_index: value.program_id_index as u8,
            accounts: value.accounts,
            data: value.data,
        }
    }
}

impl From<InnerInstruction> for generated::InnerInstruction {
    fn from(value: InnerInstruction) -> Self {
        Self {
            program_id_index: value.instruction.program_id_index as u32,
            accounts: value.instruction.accounts,
            data: value.instruction.data,
            stack_height: value.stack_height,
        }
    }
}

impl From<generated::InnerInstruction> for InnerInstruction {
    fn from(value: generated::InnerInstruction) -> Self {
        Self {
            instruction: CompiledInstruction {
                program_id_index: value.program_id_index as u8,
                accounts: value.accounts,
                data: value.data,
            },
            stack_height: value.stack_height,
        }
    }
}

impl TryFrom<tx_by_addr::TransactionError> for TransactionError {
    type Error = &'static str;

    fn try_from(transaction_error: tx_by_addr::TransactionError) -> Result<Self, Self::Error> {
        if transaction_error.transaction_error == 8 {
            if let Some(instruction_error) = transaction_error.instruction_error {
                // Decode the index.
                //
                // Index values are 32-bit integers of the form:
                // TTTTTTTT IIIIIIII xxxxxxxx xxxxxxxx
                //
                // * T - The top level index of the instruction that errored
                // * I - The inner index of the instruction that errored; 0 if None, 1-indexed otherwise
                let [outer_instruction_index, maybe_inner_instruction_index, _unused1, _unused2] =
                    instruction_error.index.to_le_bytes();
                let inner_instruction_index = maybe_inner_instruction_index.checked_sub(1);
                let responsible_program_address = instruction_error
                    .responsible_program_address_bytes
                    .map(Pubkey::try_from)
                    .transpose()
                    .expect("Failed to parse `responsible_program_address_bytes` as a Pubkey");
                if let Some(custom) = instruction_error.custom {
                    return Ok(TransactionError::InstructionError {
                        err: InstructionError::Custom(custom.custom),
                        inner_instruction_index,
                        outer_instruction_index,
                        responsible_program_address,
                    });
                }

                let ie = match instruction_error.error {
                    0 => InstructionError::GenericError,
                    1 => InstructionError::InvalidArgument,
                    2 => InstructionError::InvalidInstructionData,
                    3 => InstructionError::InvalidAccountData,
                    4 => InstructionError::AccountDataTooSmall,
                    5 => InstructionError::InsufficientFunds,
                    6 => InstructionError::IncorrectProgramId,
                    7 => InstructionError::MissingRequiredSignature,
                    8 => InstructionError::AccountAlreadyInitialized,
                    9 => InstructionError::UninitializedAccount,
                    10 => InstructionError::UnbalancedInstruction,
                    11 => InstructionError::ModifiedProgramId,
                    12 => InstructionError::ExternalAccountLamportSpend,
                    13 => InstructionError::ExternalAccountDataModified,
                    14 => InstructionError::ReadonlyLamportChange,
                    15 => InstructionError::ReadonlyDataModified,
                    16 => InstructionError::DuplicateAccountIndex,
                    17 => InstructionError::ExecutableModified,
                    18 => InstructionError::RentEpochModified,
                    19 => InstructionError::NotEnoughAccountKeys,
                    20 => InstructionError::AccountDataSizeChanged,
                    21 => InstructionError::AccountNotExecutable,
                    22 => InstructionError::AccountBorrowFailed,
                    23 => InstructionError::AccountBorrowOutstanding,
                    24 => InstructionError::DuplicateAccountOutOfSync,
                    26 => InstructionError::InvalidError,
                    27 => InstructionError::ExecutableDataModified,
                    28 => InstructionError::ExecutableLamportChange,
                    29 => InstructionError::ExecutableAccountNotRentExempt,
                    30 => InstructionError::UnsupportedProgramId,
                    31 => InstructionError::CallDepth,
                    32 => InstructionError::MissingAccount,
                    33 => InstructionError::ReentrancyNotAllowed,
                    34 => InstructionError::MaxSeedLengthExceeded,
                    35 => InstructionError::InvalidSeeds,
                    36 => InstructionError::InvalidRealloc,
                    37 => InstructionError::ComputationalBudgetExceeded,
                    38 => InstructionError::PrivilegeEscalation,
                    39 => InstructionError::ProgramEnvironmentSetupFailure,
                    40 => InstructionError::ProgramFailedToComplete,
                    41 => InstructionError::ProgramFailedToCompile,
                    42 => InstructionError::Immutable,
                    43 => InstructionError::IncorrectAuthority,
                    44 => InstructionError::BorshIoError(String::new()),
                    45 => InstructionError::AccountNotRentExempt,
                    46 => InstructionError::InvalidAccountOwner,
                    47 => InstructionError::ArithmeticOverflow,
                    48 => InstructionError::UnsupportedSysvar,
                    49 => InstructionError::IllegalOwner,
                    50 => InstructionError::MaxAccountsDataAllocationsExceeded,
                    51 => InstructionError::MaxAccountsExceeded,
                    52 => InstructionError::MaxInstructionTraceLengthExceeded,
                    53 => InstructionError::BuiltinProgramsMustConsumeComputeUnits,
                    _ => return Err("Invalid InstructionError"),
                };

                return Ok(TransactionError::InstructionError {
                    err: ie,
                    inner_instruction_index,
                    outer_instruction_index,
                    responsible_program_address,
                });
            }
        }

        if let Some(transaction_details) = transaction_error.transaction_details {
            match transaction_error.transaction_error {
                30 => {
                    return Ok(TransactionError::DuplicateInstruction(
                        transaction_details.index as u8,
                    ));
                }
                31 => {
                    return Ok(TransactionError::InsufficientFundsForRent {
                        account_index: transaction_details.index as u8,
                    });
                }

                35 => {
                    return Ok(TransactionError::ProgramExecutionTemporarilyRestricted {
                        account_index: transaction_details.index as u8,
                    });
                }
                _ => {}
            }
        }

        Ok(match transaction_error.transaction_error {
            0 => TransactionError::AccountInUse,
            1 => TransactionError::AccountLoadedTwice,
            2 => TransactionError::AccountNotFound,
            3 => TransactionError::ProgramAccountNotFound,
            4 => TransactionError::InsufficientFundsForFee,
            5 => TransactionError::InvalidAccountForFee,
            6 => TransactionError::AlreadyProcessed,
            7 => TransactionError::BlockhashNotFound,
            9 => TransactionError::CallChainTooDeep,
            10 => TransactionError::MissingSignatureForFee,
            11 => TransactionError::InvalidAccountIndex,
            12 => TransactionError::SignatureFailure,
            13 => TransactionError::InvalidProgramForExecution,
            14 => TransactionError::SanitizeFailure,
            15 => TransactionError::ClusterMaintenance,
            16 => TransactionError::AccountBorrowOutstanding,
            17 => TransactionError::WouldExceedMaxBlockCostLimit,
            18 => TransactionError::UnsupportedVersion,
            19 => TransactionError::InvalidWritableAccount,
            20 => TransactionError::WouldExceedMaxAccountCostLimit,
            21 => TransactionError::WouldExceedAccountDataBlockLimit,
            22 => TransactionError::TooManyAccountLocks,
            23 => TransactionError::AddressLookupTableNotFound,
            24 => TransactionError::InvalidAddressLookupTableOwner,
            25 => TransactionError::InvalidAddressLookupTableData,
            26 => TransactionError::InvalidAddressLookupTableIndex,
            27 => TransactionError::InvalidRentPayingAccount,
            28 => TransactionError::WouldExceedMaxVoteCostLimit,
            29 => TransactionError::WouldExceedAccountDataTotalLimit,
            32 => TransactionError::MaxLoadedAccountsDataSizeExceeded,
            33 => TransactionError::InvalidLoadedAccountsDataSizeLimit,
            34 => TransactionError::ResanitizationNeeded,
            36 => TransactionError::UnbalancedTransaction,
            37 => TransactionError::ProgramCacheHitMaxLimit,
            38 => TransactionError::CommitCancelled,
            _ => return Err("Invalid TransactionError"),
        })
    }
}

impl From<TransactionError> for tx_by_addr::TransactionError {
    fn from(transaction_error: TransactionError) -> Self {
        Self {
            transaction_error: match transaction_error {
                TransactionError::AccountInUse => tx_by_addr::TransactionErrorType::AccountInUse,
                TransactionError::AccountLoadedTwice => {
                    tx_by_addr::TransactionErrorType::AccountLoadedTwice
                }
                TransactionError::AccountNotFound => {
                    tx_by_addr::TransactionErrorType::AccountNotFound
                }
                TransactionError::ProgramAccountNotFound => {
                    tx_by_addr::TransactionErrorType::ProgramAccountNotFound
                }
                TransactionError::InsufficientFundsForFee => {
                    tx_by_addr::TransactionErrorType::InsufficientFundsForFee
                }
                TransactionError::InvalidAccountForFee => {
                    tx_by_addr::TransactionErrorType::InvalidAccountForFee
                }
                TransactionError::AlreadyProcessed => {
                    tx_by_addr::TransactionErrorType::AlreadyProcessed
                }
                TransactionError::BlockhashNotFound => {
                    tx_by_addr::TransactionErrorType::BlockhashNotFound
                }
                TransactionError::CallChainTooDeep => {
                    tx_by_addr::TransactionErrorType::CallChainTooDeep
                }
                TransactionError::MissingSignatureForFee => {
                    tx_by_addr::TransactionErrorType::MissingSignatureForFee
                }
                TransactionError::InvalidAccountIndex => {
                    tx_by_addr::TransactionErrorType::InvalidAccountIndex
                }
                TransactionError::SignatureFailure => {
                    tx_by_addr::TransactionErrorType::SignatureFailure
                }
                TransactionError::InvalidProgramForExecution => {
                    tx_by_addr::TransactionErrorType::InvalidProgramForExecution
                }
                TransactionError::SanitizeFailure => {
                    tx_by_addr::TransactionErrorType::SanitizeFailure
                }
                TransactionError::ClusterMaintenance => {
                    tx_by_addr::TransactionErrorType::ClusterMaintenance
                }
                TransactionError::InstructionError { .. } => {
                    tx_by_addr::TransactionErrorType::InstructionError
                }
                TransactionError::AccountBorrowOutstanding => {
                    tx_by_addr::TransactionErrorType::AccountBorrowOutstandingTx
                }
                TransactionError::WouldExceedMaxBlockCostLimit => {
                    tx_by_addr::TransactionErrorType::WouldExceedMaxBlockCostLimit
                }
                TransactionError::UnsupportedVersion => {
                    tx_by_addr::TransactionErrorType::UnsupportedVersion
                }
                TransactionError::InvalidWritableAccount => {
                    tx_by_addr::TransactionErrorType::InvalidWritableAccount
                }
                TransactionError::WouldExceedMaxAccountCostLimit => {
                    tx_by_addr::TransactionErrorType::WouldExceedMaxAccountCostLimit
                }
                TransactionError::WouldExceedAccountDataBlockLimit => {
                    tx_by_addr::TransactionErrorType::WouldExceedAccountDataBlockLimit
                }
                TransactionError::TooManyAccountLocks => {
                    tx_by_addr::TransactionErrorType::TooManyAccountLocks
                }
                TransactionError::AddressLookupTableNotFound => {
                    tx_by_addr::TransactionErrorType::AddressLookupTableNotFound
                }
                TransactionError::InvalidAddressLookupTableOwner => {
                    tx_by_addr::TransactionErrorType::InvalidAddressLookupTableOwner
                }
                TransactionError::InvalidAddressLookupTableData => {
                    tx_by_addr::TransactionErrorType::InvalidAddressLookupTableData
                }
                TransactionError::InvalidAddressLookupTableIndex => {
                    tx_by_addr::TransactionErrorType::InvalidAddressLookupTableIndex
                }
                TransactionError::InvalidRentPayingAccount => {
                    tx_by_addr::TransactionErrorType::InvalidRentPayingAccount
                }
                TransactionError::WouldExceedMaxVoteCostLimit => {
                    tx_by_addr::TransactionErrorType::WouldExceedMaxVoteCostLimit
                }
                TransactionError::WouldExceedAccountDataTotalLimit => {
                    tx_by_addr::TransactionErrorType::WouldExceedAccountDataTotalLimit
                }
                TransactionError::DuplicateInstruction(_) => {
                    tx_by_addr::TransactionErrorType::DuplicateInstruction
                }
                TransactionError::InsufficientFundsForRent { .. } => {
                    tx_by_addr::TransactionErrorType::InsufficientFundsForRent
                }
                TransactionError::MaxLoadedAccountsDataSizeExceeded => {
                    tx_by_addr::TransactionErrorType::MaxLoadedAccountsDataSizeExceeded
                }
                TransactionError::InvalidLoadedAccountsDataSizeLimit => {
                    tx_by_addr::TransactionErrorType::InvalidLoadedAccountsDataSizeLimit
                }
                TransactionError::ResanitizationNeeded => {
                    tx_by_addr::TransactionErrorType::ResanitizationNeeded
                }
                TransactionError::ProgramExecutionTemporarilyRestricted { .. } => {
                    tx_by_addr::TransactionErrorType::ProgramExecutionTemporarilyRestricted
                }
                TransactionError::UnbalancedTransaction => {
                    tx_by_addr::TransactionErrorType::UnbalancedTransaction
                }
                TransactionError::ProgramCacheHitMaxLimit => {
                    tx_by_addr::TransactionErrorType::ProgramCacheHitMaxLimit
                }
                TransactionError::CommitCancelled => {
                    tx_by_addr::TransactionErrorType::CommitCancelled
                }
            } as i32,
            instruction_error: match transaction_error {
                TransactionError::InstructionError {
                    err: ref instruction_error,
                    inner_instruction_index,
                    outer_instruction_index,
                    responsible_program_address,
                } => Some(tx_by_addr::InstructionError {
                    index: u32::from_le_bytes([
                        outer_instruction_index,
                        inner_instruction_index
                            .map(|i| i.checked_add(1).unwrap())
                            .unwrap_or(0),
                        0, // unused
                        0, // unused
                    ]),
                    error: match instruction_error {
                        InstructionError::GenericError => {
                            tx_by_addr::InstructionErrorType::GenericError
                        }
                        InstructionError::InvalidArgument => {
                            tx_by_addr::InstructionErrorType::InvalidArgument
                        }
                        InstructionError::InvalidInstructionData => {
                            tx_by_addr::InstructionErrorType::InvalidInstructionData
                        }
                        InstructionError::InvalidAccountData => {
                            tx_by_addr::InstructionErrorType::InvalidAccountData
                        }
                        InstructionError::AccountDataTooSmall => {
                            tx_by_addr::InstructionErrorType::AccountDataTooSmall
                        }
                        InstructionError::InsufficientFunds => {
                            tx_by_addr::InstructionErrorType::InsufficientFunds
                        }
                        InstructionError::IncorrectProgramId => {
                            tx_by_addr::InstructionErrorType::IncorrectProgramId
                        }
                        InstructionError::MissingRequiredSignature => {
                            tx_by_addr::InstructionErrorType::MissingRequiredSignature
                        }
                        InstructionError::AccountAlreadyInitialized => {
                            tx_by_addr::InstructionErrorType::AccountAlreadyInitialized
                        }
                        InstructionError::UninitializedAccount => {
                            tx_by_addr::InstructionErrorType::UninitializedAccount
                        }
                        InstructionError::UnbalancedInstruction => {
                            tx_by_addr::InstructionErrorType::UnbalancedInstruction
                        }
                        InstructionError::ModifiedProgramId => {
                            tx_by_addr::InstructionErrorType::ModifiedProgramId
                        }
                        InstructionError::ExternalAccountLamportSpend => {
                            tx_by_addr::InstructionErrorType::ExternalAccountLamportSpend
                        }
                        InstructionError::ExternalAccountDataModified => {
                            tx_by_addr::InstructionErrorType::ExternalAccountDataModified
                        }
                        InstructionError::ReadonlyLamportChange => {
                            tx_by_addr::InstructionErrorType::ReadonlyLamportChange
                        }
                        InstructionError::ReadonlyDataModified => {
                            tx_by_addr::InstructionErrorType::ReadonlyDataModified
                        }
                        InstructionError::DuplicateAccountIndex => {
                            tx_by_addr::InstructionErrorType::DuplicateAccountIndex
                        }
                        InstructionError::ExecutableModified => {
                            tx_by_addr::InstructionErrorType::ExecutableModified
                        }
                        InstructionError::RentEpochModified => {
                            tx_by_addr::InstructionErrorType::RentEpochModified
                        }
                        InstructionError::NotEnoughAccountKeys => {
                            tx_by_addr::InstructionErrorType::NotEnoughAccountKeys
                        }
                        InstructionError::AccountDataSizeChanged => {
                            tx_by_addr::InstructionErrorType::AccountDataSizeChanged
                        }
                        InstructionError::AccountNotExecutable => {
                            tx_by_addr::InstructionErrorType::AccountNotExecutable
                        }
                        InstructionError::AccountBorrowFailed => {
                            tx_by_addr::InstructionErrorType::AccountBorrowFailed
                        }
                        InstructionError::AccountBorrowOutstanding => {
                            tx_by_addr::InstructionErrorType::AccountBorrowOutstanding
                        }
                        InstructionError::DuplicateAccountOutOfSync => {
                            tx_by_addr::InstructionErrorType::DuplicateAccountOutOfSync
                        }
                        InstructionError::Custom(_) => tx_by_addr::InstructionErrorType::Custom,
                        InstructionError::InvalidError => {
                            tx_by_addr::InstructionErrorType::InvalidError
                        }
                        InstructionError::ExecutableDataModified => {
                            tx_by_addr::InstructionErrorType::ExecutableDataModified
                        }
                        InstructionError::ExecutableLamportChange => {
                            tx_by_addr::InstructionErrorType::ExecutableLamportChange
                        }
                        InstructionError::ExecutableAccountNotRentExempt => {
                            tx_by_addr::InstructionErrorType::ExecutableAccountNotRentExempt
                        }
                        InstructionError::UnsupportedProgramId => {
                            tx_by_addr::InstructionErrorType::UnsupportedProgramId
                        }
                        InstructionError::CallDepth => tx_by_addr::InstructionErrorType::CallDepth,
                        InstructionError::MissingAccount => {
                            tx_by_addr::InstructionErrorType::MissingAccount
                        }
                        InstructionError::ReentrancyNotAllowed => {
                            tx_by_addr::InstructionErrorType::ReentrancyNotAllowed
                        }
                        InstructionError::MaxSeedLengthExceeded => {
                            tx_by_addr::InstructionErrorType::MaxSeedLengthExceeded
                        }
                        InstructionError::InvalidSeeds => {
                            tx_by_addr::InstructionErrorType::InvalidSeeds
                        }
                        InstructionError::InvalidRealloc => {
                            tx_by_addr::InstructionErrorType::InvalidRealloc
                        }
                        InstructionError::ComputationalBudgetExceeded => {
                            tx_by_addr::InstructionErrorType::ComputationalBudgetExceeded
                        }
                        InstructionError::PrivilegeEscalation => {
                            tx_by_addr::InstructionErrorType::PrivilegeEscalation
                        }
                        InstructionError::ProgramEnvironmentSetupFailure => {
                            tx_by_addr::InstructionErrorType::ProgramEnvironmentSetupFailure
                        }
                        InstructionError::ProgramFailedToComplete => {
                            tx_by_addr::InstructionErrorType::ProgramFailedToComplete
                        }
                        InstructionError::ProgramFailedToCompile => {
                            tx_by_addr::InstructionErrorType::ProgramFailedToCompile
                        }
                        InstructionError::Immutable => tx_by_addr::InstructionErrorType::Immutable,
                        InstructionError::IncorrectAuthority => {
                            tx_by_addr::InstructionErrorType::IncorrectAuthority
                        }
                        InstructionError::BorshIoError(_) => {
                            tx_by_addr::InstructionErrorType::BorshIoError
                        }
                        InstructionError::AccountNotRentExempt => {
                            tx_by_addr::InstructionErrorType::AccountNotRentExempt
                        }
                        InstructionError::InvalidAccountOwner => {
                            tx_by_addr::InstructionErrorType::InvalidAccountOwner
                        }
                        InstructionError::ArithmeticOverflow => {
                            tx_by_addr::InstructionErrorType::ArithmeticOverflow
                        }
                        InstructionError::UnsupportedSysvar => {
                            tx_by_addr::InstructionErrorType::UnsupportedSysvar
                        }
                        InstructionError::IllegalOwner => {
                            tx_by_addr::InstructionErrorType::IllegalOwner
                        }
                        InstructionError::MaxAccountsDataAllocationsExceeded => {
                            tx_by_addr::InstructionErrorType::MaxAccountsDataAllocationsExceeded
                        }
                        InstructionError::MaxAccountsExceeded => {
                            tx_by_addr::InstructionErrorType::MaxAccountsExceeded
                        }
                        InstructionError::MaxInstructionTraceLengthExceeded => {
                            tx_by_addr::InstructionErrorType::MaxInstructionTraceLengthExceeded
                        }
                        InstructionError::BuiltinProgramsMustConsumeComputeUnits => {
                            tx_by_addr::InstructionErrorType::BuiltinProgramsMustConsumeComputeUnits
                        }
                    } as i32,
                    custom: match instruction_error {
                        InstructionError::Custom(custom) => {
                            Some(tx_by_addr::CustomError { custom: *custom })
                        }
                        _ => None,
                    },
                    responsible_program_address_bytes: responsible_program_address
                        .map(|p| p.to_bytes())
                        .map(Into::into),
                }),
                _ => None,
            },
            transaction_details: match transaction_error {
                TransactionError::DuplicateInstruction(index) => {
                    Some(tx_by_addr::TransactionDetails {
                        index: index as u32,
                    })
                }
                TransactionError::InsufficientFundsForRent { account_index } => {
                    Some(tx_by_addr::TransactionDetails {
                        index: account_index as u32,
                    })
                }
                TransactionError::ProgramExecutionTemporarilyRestricted { account_index } => {
                    Some(tx_by_addr::TransactionDetails {
                        index: account_index as u32,
                    })
                }

                _ => None,
            },
        }
    }
}

impl From<TransactionByAddrInfo> for tx_by_addr::TransactionByAddrInfo {
    fn from(by_addr: TransactionByAddrInfo) -> Self {
        let TransactionByAddrInfo {
            signature,
            err,
            index,
            memo,
            block_time,
        } = by_addr;

        Self {
            signature: <Signature as AsRef<[u8]>>::as_ref(&signature).into(),
            err: err.map(|e| e.into()),
            index,
            memo: memo.map(|memo| tx_by_addr::Memo { memo }),
            block_time: block_time.map(|timestamp| tx_by_addr::UnixTimestamp { timestamp }),
        }
    }
}

impl TryFrom<tx_by_addr::TransactionByAddrInfo> for TransactionByAddrInfo {
    type Error = &'static str;

    fn try_from(
        transaction_by_addr: tx_by_addr::TransactionByAddrInfo,
    ) -> Result<Self, Self::Error> {
        let err = transaction_by_addr
            .err
            .map(|err| err.try_into())
            .transpose()?;

        Ok(Self {
            signature: Signature::try_from(transaction_by_addr.signature)
                .map_err(|_| "Invalid Signature")?,
            err,
            index: transaction_by_addr.index,
            memo: transaction_by_addr
                .memo
                .map(|tx_by_addr::Memo { memo }| memo),
            block_time: transaction_by_addr
                .block_time
                .map(|tx_by_addr::UnixTimestamp { timestamp }| timestamp),
        })
    }
}

impl TryFrom<tx_by_addr::TransactionByAddr> for Vec<TransactionByAddrInfo> {
    type Error = &'static str;

    fn try_from(collection: tx_by_addr::TransactionByAddr) -> Result<Self, Self::Error> {
        collection
            .tx_by_addrs
            .into_iter()
            .map(|tx_by_addr| tx_by_addr.try_into())
            .collect::<Result<Vec<TransactionByAddrInfo>, Self::Error>>()
    }
}

impl From<(usize, EntrySummary)> for entries::Entry {
    fn from((index, entry_summary): (usize, EntrySummary)) -> Self {
        entries::Entry {
            index: index as u32,
            num_hashes: entry_summary.num_hashes,
            hash: entry_summary.hash.as_ref().into(),
            num_transactions: entry_summary.num_transactions,
            starting_transaction_index: entry_summary.starting_transaction_index as u32,
        }
    }
}

impl From<entries::Entry> for EntrySummary {
    fn from(entry: entries::Entry) -> Self {
        EntrySummary {
            num_hashes: entry.num_hashes,
            hash: <[u8; HASH_BYTES]>::try_from(entry.hash)
                .map(Hash::new_from_array)
                .unwrap(),
            num_transactions: entry.num_transactions,
            starting_transaction_index: entry.starting_transaction_index as usize,
        }
    }
}

#[cfg(test)]
mod test {
    use {super::*, bincode, enum_iterator::all, serde_derive::Deserialize, test_case::test_case};

    #[test]
    fn test_reward_type_encode() {
        let mut reward = Reward {
            pubkey: "invalid".to_string(),
            lamports: 123,
            post_balance: 321,
            reward_type: None,
            commission: None,
        };
        let gen_reward: generated::Reward = reward.clone().into();
        assert_eq!(reward, gen_reward.into());

        reward.reward_type = Some(RewardType::Fee);
        let gen_reward: generated::Reward = reward.clone().into();
        assert_eq!(reward, gen_reward.into());

        reward.reward_type = Some(RewardType::Rent);
        let gen_reward: generated::Reward = reward.clone().into();
        assert_eq!(reward, gen_reward.into());

        reward.reward_type = Some(RewardType::Voting);
        let gen_reward: generated::Reward = reward.clone().into();
        assert_eq!(reward, gen_reward.into());

        reward.reward_type = Some(RewardType::Staking);
        let gen_reward: generated::Reward = reward.clone().into();
        assert_eq!(reward, gen_reward.into());
    }

    #[test]
    fn test_transaction_by_addr_encode() {
        let info = TransactionByAddrInfo {
            signature: bs58::decode("Nfo6rgemG1KLbk1xuNwfrQTsdxaGfLuWURHNRy9LYnDrubG7LFQZaA5obPNas9LQ6DdorJqxh2LxA3PsnWdkSrL")
                .into_vec()
                .map(Signature::try_from)
                .unwrap()
                .unwrap(),
            err: None,
            index: 5,
            memo: Some("string".to_string()),
            block_time: Some(1610674861)
        };

        let tx_by_addr_transaction_info: tx_by_addr::TransactionByAddrInfo = info.clone().into();
        assert_eq!(info, tx_by_addr_transaction_info.try_into().unwrap());
    }

    #[test]
    fn test_transaction_error_encode() {
        let transaction_error = TransactionError::AccountBorrowOutstanding;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::AccountInUse;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::AccountLoadedTwice;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::AccountNotFound;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::AlreadyProcessed;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::BlockhashNotFound;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::CallChainTooDeep;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::ClusterMaintenance;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InsufficientFundsForFee;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InvalidAccountForFee;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InvalidAccountIndex;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InvalidProgramForExecution;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::MissingSignatureForFee;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::ProgramAccountNotFound;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::SanitizeFailure;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::SignatureFailure;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::WouldExceedMaxBlockCostLimit;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::WouldExceedMaxVoteCostLimit;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::WouldExceedMaxAccountCostLimit;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::UnsupportedVersion;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::AccountAlreadyInitialized,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::AccountBorrowFailed,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::AccountBorrowOutstanding,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::AccountDataSizeChanged,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::AccountDataTooSmall,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::AccountNotExecutable,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            err: InstructionError::CallDepth,
            inner_instruction_index: Some(42),
            outer_instruction_index: 10,
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ComputationalBudgetExceeded,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::DuplicateAccountIndex,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::DuplicateAccountOutOfSync,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ExecutableAccountNotRentExempt,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ExecutableDataModified,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ExecutableLamportChange,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ExecutableModified,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ExternalAccountDataModified,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ExternalAccountLamportSpend,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::GenericError,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            err: InstructionError::Immutable,
            inner_instruction_index: Some(42),
            outer_instruction_index: 10,
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::IncorrectAuthority,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::IncorrectProgramId,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::InsufficientFunds,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::InvalidAccountData,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::InvalidArgument,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::InvalidError,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::InvalidInstructionData,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::InvalidRealloc,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::InvalidSeeds,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::MaxSeedLengthExceeded,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::MissingAccount,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::MissingRequiredSignature,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ModifiedProgramId,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            err: InstructionError::NotEnoughAccountKeys,
            inner_instruction_index: None,
            outer_instruction_index: 10,
            responsible_program_address: None,
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::PrivilegeEscalation,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ProgramEnvironmentSetupFailure,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ProgramFailedToCompile,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ProgramFailedToComplete,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ReadonlyDataModified,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ReadonlyLamportChange,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::ReentrancyNotAllowed,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::RentEpochModified,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::UnbalancedInstruction,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::UninitializedAccount,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            outer_instruction_index: 10,
            err: InstructionError::UnsupportedProgramId,
            inner_instruction_index: Some(41),
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InstructionError {
            err: InstructionError::Custom(10),
            inner_instruction_index: Some(42),
            outer_instruction_index: 10,
            responsible_program_address: Some(Pubkey::new_unique()),
        };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::DuplicateInstruction(10);
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::InsufficientFundsForRent { account_index: 10 };
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );

        let transaction_error = TransactionError::UnbalancedTransaction;
        let tx_by_addr_transaction_error: tx_by_addr::TransactionError =
            transaction_error.clone().into();
        assert_eq!(
            transaction_error,
            tx_by_addr_transaction_error.try_into().unwrap()
        );
    }

    #[test]
    fn test_error_enums() {
        let ix_index: u32 = 1;
        let custom_error = 42;
        for error in all::<tx_by_addr::TransactionErrorType>() {
            match error {
                tx_by_addr::TransactionErrorType::DuplicateInstruction
                | tx_by_addr::TransactionErrorType::InsufficientFundsForRent
                | tx_by_addr::TransactionErrorType::ProgramExecutionTemporarilyRestricted => {
                    let tx_by_addr_error = tx_by_addr::TransactionError {
                        transaction_error: error as i32,
                        instruction_error: None,
                        transaction_details: Some(tx_by_addr::TransactionDetails {
                            index: ix_index,
                        }),
                    };
                    let transaction_error: TransactionError = tx_by_addr_error
                        .clone()
                        .try_into()
                        .unwrap_or_else(|_| panic!("{error:?} conversion implemented?"));
                    assert_eq!(tx_by_addr_error, transaction_error.into());
                }
                tx_by_addr::TransactionErrorType::InstructionError => {
                    for ix_error in all::<tx_by_addr::InstructionErrorType>() {
                        if ix_error != tx_by_addr::InstructionErrorType::Custom {
                            let tx_by_addr_error = tx_by_addr::TransactionError {
                                transaction_error: error as i32,
                                instruction_error: Some(tx_by_addr::InstructionError {
                                    index: ix_index,
                                    error: ix_error as i32,
                                    custom: None,
                                    responsible_program_address_bytes: Some(
                                        Pubkey::new_unique().as_array().into(),
                                    ),
                                }),
                                transaction_details: None,
                            };
                            let transaction_error: TransactionError = tx_by_addr_error
                                .clone()
                                .try_into()
                                .unwrap_or_else(|_| panic!("{ix_error:?} conversion implemented?"));
                            assert_eq!(tx_by_addr_error, transaction_error.into());
                        } else {
                            let tx_by_addr_error = tx_by_addr::TransactionError {
                                transaction_error: error as i32,
                                instruction_error: Some(tx_by_addr::InstructionError {
                                    index: ix_index,
                                    error: ix_error as i32,
                                    custom: Some(tx_by_addr::CustomError {
                                        custom: custom_error,
                                    }),
                                    responsible_program_address_bytes: Some(
                                        Pubkey::new_unique().as_array().into(),
                                    ),
                                }),
                                transaction_details: None,
                            };
                            let transaction_error: TransactionError =
                                tx_by_addr_error.clone().try_into().unwrap();
                            assert_eq!(tx_by_addr_error, transaction_error.into());
                        }
                    }
                }
                _ => {
                    let tx_by_addr_error = tx_by_addr::TransactionError {
                        transaction_error: error as i32,
                        instruction_error: None,
                        transaction_details: None,
                    };
                    let transaction_error: TransactionError = tx_by_addr_error
                        .clone()
                        .try_into()
                        .unwrap_or_else(|_| panic!("{error:?} conversion implemented?"));
                    assert_eq!(tx_by_addr_error, transaction_error.into());
                }
            }
        }
    }

    #[test_case(TransactionError::InsufficientFundsForFee; "Typical")]
    #[test_case(TransactionError::InstructionError{
        err: InstructionError::Custom(0xdeadbeef),
        inner_instruction_index: Some(41),
        outer_instruction_index: 42,
        responsible_program_address: Some(Pubkey::new_unique()),
    }; "Special case (InstructionError)")]
    fn test_serialize_transaction_error_to_generated_transaction_error_round_trip(
        err: TransactionError,
    ) {
        let serialized: generated::TransactionError = err.clone().into();
        let deserialized: TransactionError = serialized.into();
        assert_eq!(deserialized, err);
    }

    #[test_case(
        vec![
            8, 0, 0, 0, /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            /* Missing data that was introduced in Agave 2.3 */
        ],
        TransactionError::InstructionError {
            err: InstructionError::Custom(0xdeadbeef),
            inner_instruction_index: None,
            outer_instruction_index: 42,
            responsible_program_address: None,
        };
        "Legacy error with only outer instruction index and error"
    )]
    #[test_case(
        vec![
            8, 0, 0, 0, /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            0, /* Responsible program address - None */
            0, /* Inner instruction index - None */
        ],
        TransactionError::InstructionError {
            err: InstructionError::Custom(0xdeadbeef),
            inner_instruction_index: None,
            outer_instruction_index: 42,
            responsible_program_address: None,
        };
        "Modern error with only outer instruction index and error"
    )]
    #[test_case(
        vec![
            8, 0, 0, 0, /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            /* Responsible program address - Some(4wBqpZM9xaSheZzJSMawUKKwhdpChKbZ5eu5ky4Vigw) */
            1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
            0, /* Inner instruction index - None */
        ],
        TransactionError::InstructionError {
            err: InstructionError::Custom(0xdeadbeef),
            inner_instruction_index: None,
            outer_instruction_index: 42,
            responsible_program_address: Some(Pubkey::from_str_const("4wBqpZM9xaSheZzJSMawUKKwhdpChKbZ5eu5ky4Vigw")),
        };
        "Modern error with outer instruction index, error, and responsible program address"
    )]
    #[test_case(
        vec![
            8, 0, 0, 0, /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            0, /* Responsible program address - None */
            1, 41, /* Inner instruction index - Some(41) */
        ],
        TransactionError::InstructionError {
            err: InstructionError::Custom(0xdeadbeef),
            inner_instruction_index: Some(41),
            outer_instruction_index: 42,
            responsible_program_address: None,
        };
        "Modern error with outer instruction index, error, and inner instruction index"
    )]
    #[test_case(
        vec![
            8, 0, 0, 0, /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            /* Responsible program address - Some(4wBqpZM9xaSheZzJSMawUKKwhdpChKbZ5eu5ky4Vigw) */
            1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
            1, 41, /* Inner instruction index - Some(41) */
        ],
        TransactionError::InstructionError {
            err: InstructionError::Custom(0xdeadbeef),
            inner_instruction_index: Some(41),
            outer_instruction_index: 42,
            responsible_program_address: Some(Pubkey::from_str_const("4wBqpZM9xaSheZzJSMawUKKwhdpChKbZ5eu5ky4Vigw")),
        };
        "Modern error with everything"
    )]
    fn test_deserialize_transaction_error_instruction_error_forward_compatibility(
        stored_transaction_error_bytes: Vec<u8>,
        expected_transaction_error: TransactionError,
    ) {
        let legacy_stored_transaction_error = generated::TransactionError {
            err: stored_transaction_error_bytes,
        };
        let deserialized: TransactionError = legacy_stored_transaction_error.into();
        assert_eq!(deserialized, expected_transaction_error,);
    }

    #[test]
    /// This test exists to ensure that if we store data in blockstore in the modern format that it
    /// can still be read by legacy software. The legacy software should simply ignore the new data
    /// that comes after what it believes to be the EOF.
    fn test_deserialize_transaction_error_instruction_error_backward_compatibility() {
        #[derive(Debug, Deserialize, PartialEq)]
        enum LegacyTransactionError {
            Placeholder0,
            Placeholder1,
            Placeholder2,
            Placeholder3,
            Placeholder4,
            Placeholder5,
            Placeholder6,
            Placeholder7,
            InstructionError(u8, InstructionError),
        }
        let modern_stored_transaction_error_bytes = vec![
            8, 0, 0, 0,  /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            0, 0, /* Extra stuff the legacy enum does not expect */
        ];
        let deserialized: LegacyTransactionError =
            bincode::deserialize(&modern_stored_transaction_error_bytes)
                .expect("Failed to deserialize modern byte format to legacy transaction error");
        assert_eq!(
            deserialized,
            LegacyTransactionError::InstructionError(42, InstructionError::Custom(0xdeadbeef)),
        );
    }

    #[test_case(
        vec![
            8, 0, 0, 0, /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            0, /* Responsible program address - None */
            0, /* Inner instruction index - None */
        ],
        TransactionError::InstructionError {
            err: InstructionError::Custom(0xdeadbeef),
            inner_instruction_index: None,
            outer_instruction_index: 42,
            responsible_program_address: None,
        };
        "Outer instruction index and error"
    )]
    #[test_case(
        vec![
            8, 0, 0, 0, /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            /* Responsible program address - Some(4wBqpZM9xaSheZzJSMawUKKwhdpChKbZ5eu5ky4Vigw) */
            1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
            0, /* Inner instruction index - None */
        ],
        TransactionError::InstructionError {
            err: InstructionError::Custom(0xdeadbeef),
            inner_instruction_index: None,
            outer_instruction_index: 42,
            responsible_program_address: Some(Pubkey::from_str_const("4wBqpZM9xaSheZzJSMawUKKwhdpChKbZ5eu5ky4Vigw")),
        };
        "Outer instruction index, error, and responsible program address"
    )]
    #[test_case(
        vec![
            8, 0, 0, 0, /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            0, /* Responsible program address - None */
            1, 41, /* Inner instruction index - Some(41) */
        ],
        TransactionError::InstructionError {
            err: InstructionError::Custom(0xdeadbeef),
            inner_instruction_index: Some(41),
            outer_instruction_index: 42,
            responsible_program_address: None,
        };
        "Outer instruction index, error, and inner instruction index"
    )]
    #[test_case(
        vec![
            8, 0, 0, 0, /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, 239, 190, 173, 222, /* InstructionError::Custom(0xdeadbeef) */
            /* Responsible program address - Some(4wBqpZM9xaSheZzJSMawUKKwhdpChKbZ5eu5ky4Vigw) */
            1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
            1, 41, /* Inner instruction index - Some(41) */
        ],
        TransactionError::InstructionError {
            err: InstructionError::Custom(0xdeadbeef),
            inner_instruction_index: Some(41),
            outer_instruction_index: 42,
            responsible_program_address: Some(Pubkey::from_str_const("4wBqpZM9xaSheZzJSMawUKKwhdpChKbZ5eu5ky4Vigw")),
        };
        "Error with everything"
    )]
    fn test_serialize_transaction_error_instruction_error_backward_compatibility(
        expected_stored_transaction_error_bytes: Vec<u8>,
        transaction_error: TransactionError,
    ) {
        let stored_transaction_error: StoredTransactionError = transaction_error.into();
        assert_eq!(
            stored_transaction_error.0,
            expected_stored_transaction_error_bytes,
        );
    }
}
