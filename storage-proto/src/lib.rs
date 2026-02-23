#![cfg(feature = "agave-unstable-api")]
use {
    serde::{Deserialize, Serialize},
    solana_account_decoder::{
        StringAmount,
        parse_token::{UiTokenAmount, real_number_string_trimmed},
    },
    solana_message::v0::LoadedAddresses,
    solana_serde::default_on_eof,
    solana_transaction_context::transaction::TransactionReturnData,
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    solana_transaction_status::{
        InnerInstructions, Reward, RewardType, TransactionStatusMeta, TransactionTokenBalance,
    },
    std::str::FromStr,
};

pub mod convert;

pub type StoredExtendedRewards = Vec<StoredExtendedReward>;

#[derive(Serialize, Deserialize)]
pub struct StoredExtendedReward {
    pubkey: String,
    lamports: i64,
    #[serde(deserialize_with = "default_on_eof")]
    post_balance: u64,
    #[serde(deserialize_with = "default_on_eof")]
    reward_type: Option<RewardType>,
    #[serde(deserialize_with = "default_on_eof")]
    commission: Option<u8>,
    #[serde(deserialize_with = "default_on_eof")]
    commission_bps: Option<u16>,
}

impl From<StoredExtendedReward> for Reward {
    fn from(value: StoredExtendedReward) -> Self {
        let StoredExtendedReward {
            pubkey,
            lamports,
            post_balance,
            reward_type,
            commission,
            commission_bps,
        } = value;
        Self {
            pubkey,
            lamports,
            post_balance,
            reward_type,
            commission,
            commission_bps,
        }
    }
}

impl From<Reward> for StoredExtendedReward {
    fn from(value: Reward) -> Self {
        let Reward {
            pubkey,
            lamports,
            post_balance,
            reward_type,
            commission,
            commission_bps,
        } = value;
        Self {
            pubkey,
            lamports,
            post_balance,
            reward_type,
            commission,
            commission_bps,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct StoredTokenAmount {
    pub ui_amount: f64,
    pub decimals: u8,
    pub amount: StringAmount,
}

impl From<StoredTokenAmount> for UiTokenAmount {
    fn from(value: StoredTokenAmount) -> Self {
        let StoredTokenAmount {
            ui_amount,
            decimals,
            amount,
        } = value;
        let ui_amount_string =
            real_number_string_trimmed(u64::from_str(&amount).unwrap_or(0), decimals);
        Self {
            ui_amount: Some(ui_amount),
            decimals,
            amount,
            ui_amount_string,
        }
    }
}

impl From<UiTokenAmount> for StoredTokenAmount {
    fn from(value: UiTokenAmount) -> Self {
        let UiTokenAmount {
            ui_amount,
            decimals,
            amount,
            ..
        } = value;
        Self {
            ui_amount: ui_amount.unwrap_or(0.0),
            decimals,
            amount,
        }
    }
}

struct StoredTransactionError(Vec<u8>);

impl From<StoredTransactionError> for TransactionError {
    fn from(value: StoredTransactionError) -> Self {
        let bytes = value.0;
        bincode::deserialize(&bytes).expect("transaction error to deserialize from bytes")
    }
}

impl From<TransactionError> for StoredTransactionError {
    fn from(value: TransactionError) -> Self {
        let bytes = bincode::serialize(&value).expect("transaction error to serialize to bytes");
        StoredTransactionError(bytes)
    }
}

#[derive(Serialize, Deserialize)]
pub struct StoredTransactionTokenBalance {
    pub account_index: u8,
    pub mint: String,
    pub ui_token_amount: StoredTokenAmount,
    #[serde(deserialize_with = "default_on_eof")]
    pub owner: String,
    #[serde(deserialize_with = "default_on_eof")]
    pub program_id: String,
}

impl From<StoredTransactionTokenBalance> for TransactionTokenBalance {
    fn from(value: StoredTransactionTokenBalance) -> Self {
        let StoredTransactionTokenBalance {
            account_index,
            mint,
            ui_token_amount,
            owner,
            program_id,
        } = value;
        Self {
            account_index,
            mint,
            ui_token_amount: ui_token_amount.into(),
            owner,
            program_id,
        }
    }
}

impl From<TransactionTokenBalance> for StoredTransactionTokenBalance {
    fn from(value: TransactionTokenBalance) -> Self {
        let TransactionTokenBalance {
            account_index,
            mint,
            ui_token_amount,
            owner,
            program_id,
        } = value;
        Self {
            account_index,
            mint,
            ui_token_amount: ui_token_amount.into(),
            owner,
            program_id,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct StoredTransactionStatusMeta {
    pub status: Result<()>,
    pub fee: u64,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    #[serde(deserialize_with = "default_on_eof")]
    pub inner_instructions: Option<Vec<InnerInstructions>>,
    #[serde(deserialize_with = "default_on_eof")]
    pub log_messages: Option<Vec<String>>,
    #[serde(deserialize_with = "default_on_eof")]
    pub pre_token_balances: Option<Vec<StoredTransactionTokenBalance>>,
    #[serde(deserialize_with = "default_on_eof")]
    pub post_token_balances: Option<Vec<StoredTransactionTokenBalance>>,
    #[serde(deserialize_with = "default_on_eof")]
    pub rewards: Option<Vec<StoredExtendedReward>>,
    #[serde(deserialize_with = "default_on_eof")]
    pub return_data: Option<TransactionReturnData>,
    #[serde(deserialize_with = "default_on_eof")]
    pub compute_units_consumed: Option<u64>,
    #[serde(deserialize_with = "default_on_eof")]
    pub cost_units: Option<u64>,
}

impl From<StoredTransactionStatusMeta> for TransactionStatusMeta {
    fn from(value: StoredTransactionStatusMeta) -> Self {
        let StoredTransactionStatusMeta {
            status,
            fee,
            pre_balances,
            post_balances,
            inner_instructions,
            log_messages,
            pre_token_balances,
            post_token_balances,
            rewards,
            return_data,
            compute_units_consumed,
            cost_units,
        } = value;
        Self {
            status,
            fee,
            pre_balances,
            post_balances,
            inner_instructions,
            log_messages,
            pre_token_balances: pre_token_balances
                .map(|balances| balances.into_iter().map(|balance| balance.into()).collect()),
            post_token_balances: post_token_balances
                .map(|balances| balances.into_iter().map(|balance| balance.into()).collect()),
            rewards: rewards
                .map(|rewards| rewards.into_iter().map(|reward| reward.into()).collect()),
            loaded_addresses: LoadedAddresses::default(),
            return_data,
            compute_units_consumed,
            cost_units,
        }
    }
}

impl TryFrom<TransactionStatusMeta> for StoredTransactionStatusMeta {
    type Error = bincode::Error;
    fn try_from(value: TransactionStatusMeta) -> std::result::Result<Self, Self::Error> {
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

        if !loaded_addresses.is_empty() {
            // Deprecated bincode serialized status metadata doesn't support
            // loaded addresses.
            return Err(
                bincode::ErrorKind::Custom("Bincode serialization is deprecated".into()).into(),
            );
        }

        Ok(Self {
            status,
            fee,
            pre_balances,
            post_balances,
            inner_instructions,
            log_messages,
            pre_token_balances: pre_token_balances
                .map(|balances| balances.into_iter().map(|balance| balance.into()).collect()),
            post_token_balances: post_token_balances
                .map(|balances| balances.into_iter().map(|balance| balance.into()).collect()),
            rewards: rewards
                .map(|rewards| rewards.into_iter().map(|reward| reward.into()).collect()),
            return_data,
            compute_units_consumed,
            cost_units,
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::StoredTransactionError, solana_instruction::error::InstructionError,
        solana_transaction_error::TransactionError, test_case::test_case,
    };

    #[test_case(TransactionError::InsufficientFundsForFee; "Named variant error")]
    #[test_case(TransactionError::InsufficientFundsForRent { account_index: 42 }; "Struct variant error")]
    #[test_case(TransactionError::DuplicateInstruction(42); "Single-value tuple variant error")]
    #[test_case(TransactionError::InstructionError(42, InstructionError::Custom(0xdeadbeef)); "`InstructionError`")]
    fn test_serialize_transaction_error_to_stored_transaction_error_round_trip(
        err: TransactionError,
    ) {
        let serialized: StoredTransactionError = err.clone().into();
        let deserialized: TransactionError = serialized.into();
        assert_eq!(deserialized, err);
    }

    #[test_case(
        vec![4, 0, 0, 0,  /* Fourth enum variant - `InsufficientFundsForFee` */],
        TransactionError::InsufficientFundsForFee;
        "Named variant error"
    )]
    #[test_case(
        vec![
            31, 0, 0, 0,  /* Thirty-first enum variant - `InsufficientFundsForRent` */
            42, /* Account index */
        ],
        TransactionError::InsufficientFundsForRent { account_index: 42 };
        "Struct variant error"
    )]
    #[test_case(
        vec![
            30, 0, 0, 0,  /* Thirtieth enum variant - `DuplicateInstruction` */
            42, /* Instruction index */
        ],
        TransactionError::DuplicateInstruction(42);
        "Single-value tuple variant error"
    )]
    #[test_case(
        vec![
            8, 0, 0, 0,  /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, /* InstructionError::Custom */
            /* 0xdeadbeef */
            239, 190, 173, 222,
        ],
        TransactionError::InstructionError(42, InstructionError::Custom(0xdeadbeef));
        "`InstructionError`"
    )]
    fn test_deserialize_stored_transaction_error(
        stored_bytes: Vec<u8>,
        expected_transaction_error: TransactionError,
    ) {
        let stored_transaction = StoredTransactionError(stored_bytes);
        let deserialized: TransactionError = stored_transaction.into();
        assert_eq!(deserialized, expected_transaction_error);
    }

    #[test_case(
        vec![4, 0, 0, 0,  /* Fourth enum variant - `InsufficientFundsForFee` */],
        TransactionError::InsufficientFundsForFee;
        "Named variant error"
    )]
    #[test_case(
        vec![
            31, 0, 0, 0,  /* Thirty-first enum variant - `InsufficientFundsForRent` */
            42, /* Account index */
        ],
        TransactionError::InsufficientFundsForRent { account_index: 42 };
        "Struct variant error"
    )]
    #[test_case(
        vec![
            30, 0, 0, 0,  /* Thirtieth enum variant - `DuplicateInstruction` */
            42, /* Instruction index */
        ],
        TransactionError::DuplicateInstruction(42);
        "Single-value tuple variant error"
    )]
    #[test_case(
        vec![
            8, 0, 0, 0,  /* Eighth enum variant - `InstructionError` */
            42, /* Outer instruction index */
            25, 0, 0, 0, /* InstructionError::Custom */
            /* 0xdeadbeef */
            239, 190, 173, 222,
        ],
        TransactionError::InstructionError(42, InstructionError::Custom(0xdeadbeef));
        "`InstructionError`"
    )]
    fn test_seserialize_stored_transaction_error(
        expected_serialized_bytes: Vec<u8>,
        transaction_error: TransactionError,
    ) {
        let StoredTransactionError(serialized_bytes) = transaction_error.into();
        assert_eq!(serialized_bytes, expected_serialized_bytes);
    }
}
