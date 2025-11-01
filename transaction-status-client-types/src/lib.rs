#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
//! Core types for solana-transaction-status
use {
    crate::option_serializer::OptionSerializer,
    base64::{prelude::BASE64_STANDARD, Engine},
    core::fmt,
    serde::{
        de::{self, Deserialize as DeserializeTrait, Error as DeserializeError},
        ser::{Serialize as SerializeTrait, SerializeTupleVariant},
        Deserialize, Deserializer, Serialize,
    },
    serde_json::{from_value, Value},
    solana_account_decoder_client_types::token::UiTokenAmount,
    solana_commitment_config::CommitmentConfig,
    solana_instruction::error::InstructionError,
    solana_message::{
        compiled_instruction::CompiledInstruction,
        v0::{LoadedAddresses, MessageAddressTableLookup},
        MessageHeader,
    },
    solana_reward_info::RewardType,
    solana_signature::Signature,
    solana_transaction::versioned::{TransactionVersion, VersionedTransaction},
    solana_transaction_context::TransactionReturnData,
    solana_transaction_error::{TransactionError, TransactionResult},
    thiserror::Error,
};
pub mod option_serializer;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TransactionBinaryEncoding {
    Base58,
    Base64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum UiTransactionEncoding {
    Binary, // Legacy. Retained for RPC backwards compatibility
    Base64,
    Base58,
    Json,
    JsonParsed,
}

impl UiTransactionEncoding {
    pub fn into_binary_encoding(&self) -> Option<TransactionBinaryEncoding> {
        match self {
            Self::Binary | Self::Base58 => Some(TransactionBinaryEncoding::Base58),
            Self::Base64 => Some(TransactionBinaryEncoding::Base64),
            _ => None,
        }
    }
}

impl fmt::Display for UiTransactionEncoding {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let v = serde_json::to_value(self).map_err(|_| fmt::Error)?;
        let s = v.as_str().ok_or(fmt::Error)?;
        write!(f, "{s}")
    }
}

#[derive(Default, Debug, Clone, Copy, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionDetails {
    #[default]
    Full,
    Signatures,
    None,
    Accounts,
}

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum EncodeError {
    #[error("Encoding does not support transaction version {0}")]
    UnsupportedTransactionVersion(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmedTransactionStatusWithSignature {
    pub signature: Signature,
    pub slot: u64,
    pub err: Option<TransactionError>,
    pub memo: Option<String>,
    pub block_time: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionConfirmationStatus {
    Processed,
    Confirmed,
    Finalized,
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UiConfirmedBlock {
    pub previous_blockhash: String,
    pub blockhash: String,
    pub parent_slot: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transactions: Option<Vec<EncodedTransactionWithStatusMeta>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signatures: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewards: Option<Rewards>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_reward_partitions: Option<u64>,
    pub block_time: Option<i64>,
    pub block_height: Option<u64>,
}

/// A duplicate representation of a Transaction for pretty JSON serialization
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiTransaction {
    pub signatures: Vec<String>,
    pub message: UiMessage,
}

/// A duplicate representation of a Message, in parsed format, for pretty JSON serialization
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiParsedMessage {
    pub account_keys: Vec<ParsedAccount>,
    pub recent_blockhash: String,
    pub instructions: Vec<UiInstruction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address_table_lookups: Option<Vec<UiAddressTableLookup>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedAccount {
    pub pubkey: String,
    pub writable: bool,
    pub signer: bool,
    pub source: Option<ParsedAccountSource>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ParsedAccountSource {
    Transaction,
    LookupTable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum UiMessage {
    Parsed(UiParsedMessage),
    Raw(UiRawMessage),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum EncodedTransaction {
    LegacyBinary(String), // Old way of expressing base-58, retained for RPC backwards compatibility
    Binary(String, TransactionBinaryEncoding),
    Json(UiTransaction),
    Accounts(UiAccountsList),
}

impl EncodedTransaction {
    pub fn decode(&self) -> Option<VersionedTransaction> {
        let (blob, encoding) = match self {
            Self::Json(_) | Self::Accounts(_) => return None,
            Self::LegacyBinary(blob) => (blob, TransactionBinaryEncoding::Base58),
            Self::Binary(blob, encoding) => (blob, *encoding),
        };

        let transaction: Option<VersionedTransaction> = match encoding {
            TransactionBinaryEncoding::Base58 => bs58::decode(blob)
                .into_vec()
                .ok()
                .and_then(|bytes| bincode::deserialize(&bytes).ok()),
            TransactionBinaryEncoding::Base64 => BASE64_STANDARD
                .decode(blob)
                .ok()
                .and_then(|bytes| bincode::deserialize(&bytes).ok()),
        };

        transaction.filter(|transaction| transaction.sanitize().is_ok())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodedTransactionWithStatusMeta {
    pub transaction: EncodedTransaction,
    pub meta: Option<UiTransactionStatusMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<TransactionVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reward {
    pub pubkey: String,
    pub lamports: i64,
    pub post_balance: u64, // Account balance in lamports after `lamports` was applied
    pub reward_type: Option<RewardType>,
    pub commission: Option<u8>, // Vote account commission when the reward was credited, only present for voting and staking rewards
}

pub type Rewards = Vec<Reward>;

/// A duplicate representation of a MessageAddressTableLookup, in raw format, for pretty JSON serialization
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiAddressTableLookup {
    pub account_key: String,
    pub writable_indexes: Vec<u8>,
    pub readonly_indexes: Vec<u8>,
}

impl From<&MessageAddressTableLookup> for UiAddressTableLookup {
    fn from(lookup: &MessageAddressTableLookup) -> Self {
        Self {
            account_key: lookup.account_key.to_string(),
            writable_indexes: lookup.writable_indexes.clone(),
            readonly_indexes: lookup.readonly_indexes.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiTransactionError(TransactionError);

impl fmt::Display for UiTransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for UiTransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<TransactionError> for UiTransactionError {
    fn from(value: TransactionError) -> Self {
        UiTransactionError(value)
    }
}

impl From<UiTransactionError> for TransactionError {
    fn from(value: UiTransactionError) -> Self {
        value.0
    }
}

impl SerializeTrait for UiTransactionError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match &self.0 {
            TransactionError::InstructionError(outer_instruction_index, err) => {
                let mut state = serializer.serialize_tuple_variant(
                    "TransactionError",
                    8,
                    "InstructionError",
                    2,
                )?;
                state.serialize_field(outer_instruction_index)?;
                state.serialize_field(err)?;
                state.end()
            }
            err => TransactionError::serialize(err, serializer),
        }
    }
}

impl<'de> DeserializeTrait<'de> for UiTransactionError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(obj) = value.as_object() {
            if let Some(arr) = obj.get("InstructionError").and_then(|v| v.as_array()) {
                let outer_instruction_index: u8 = arr
                    .first()
                    .ok_or_else(|| {
                        DeserializeError::invalid_length(0, &"Expected the first element to exist")
                    })?
                    .as_u64()
                    .ok_or_else(|| {
                        DeserializeError::custom("Expected the first element to be a u64")
                    })? as u8;
                let instruction_error = arr.get(1).ok_or_else(|| {
                    DeserializeError::invalid_length(1, &"Expected there to be at least 2 elements")
                })?;

                // Handle SDK version compatibility: if it's a v2-style
                // {"BorshIoError": "Unknown"}, convert it to a v3-style
                // "BorshIoError"
                let err: InstructionError = if instruction_error.get("BorshIoError").is_some() {
                    from_value(serde_json::json!("BorshIoError"))
                } else {
                    from_value(instruction_error.clone())
                }
                .map_err(|e| DeserializeError::custom(e.to_string()))?;
                return Ok(UiTransactionError(TransactionError::InstructionError(
                    outer_instruction_index,
                    err,
                )));
            }
        }
        let err = TransactionError::deserialize(value).map_err(de::Error::custom)?;
        Ok(UiTransactionError(err))
    }
}

/// A duplicate representation of TransactionStatusMeta with `err` field
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiTransactionStatusMeta {
    pub err: Option<UiTransactionError>,
    pub status: Result<(), UiTransactionError>, // This field is deprecated.  See https://github.com/solana-labs/solana/issues/9302
    pub fee: u64,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    #[serde(
        default = "OptionSerializer::none",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub inner_instructions: OptionSerializer<Vec<UiInnerInstructions>>,
    #[serde(
        default = "OptionSerializer::none",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub log_messages: OptionSerializer<Vec<String>>,
    #[serde(
        default = "OptionSerializer::none",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub pre_token_balances: OptionSerializer<Vec<UiTransactionTokenBalance>>,
    #[serde(
        default = "OptionSerializer::none",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub post_token_balances: OptionSerializer<Vec<UiTransactionTokenBalance>>,
    #[serde(
        default = "OptionSerializer::none",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub rewards: OptionSerializer<Rewards>,
    #[serde(
        default = "OptionSerializer::skip",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub loaded_addresses: OptionSerializer<UiLoadedAddresses>,
    #[serde(
        default = "OptionSerializer::skip",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub return_data: OptionSerializer<UiTransactionReturnData>,
    #[serde(
        default = "OptionSerializer::skip",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub compute_units_consumed: OptionSerializer<u64>,
    #[serde(
        default = "OptionSerializer::skip",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub cost_units: OptionSerializer<u64>,
}

impl From<TransactionStatusMeta> for UiTransactionStatusMeta {
    fn from(meta: TransactionStatusMeta) -> Self {
        Self {
            err: meta.status.clone().map_err(Into::into).err(),
            status: meta.status.map_err(Into::into),
            fee: meta.fee,
            pre_balances: meta.pre_balances,
            post_balances: meta.post_balances,
            inner_instructions: meta
                .inner_instructions
                .map(|ixs| ixs.into_iter().map(Into::into).collect())
                .into(),
            log_messages: meta.log_messages.into(),
            pre_token_balances: meta
                .pre_token_balances
                .map(|balance| balance.into_iter().map(Into::into).collect())
                .into(),
            post_token_balances: meta
                .post_token_balances
                .map(|balance| balance.into_iter().map(Into::into).collect())
                .into(),
            rewards: meta.rewards.into(),
            loaded_addresses: Some(UiLoadedAddresses::from(&meta.loaded_addresses)).into(),
            return_data: OptionSerializer::or_skip(
                meta.return_data.map(|return_data| return_data.into()),
            ),
            compute_units_consumed: OptionSerializer::or_skip(meta.compute_units_consumed),
            cost_units: OptionSerializer::or_skip(meta.cost_units),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UiTransactionReturnData {
    pub program_id: String,
    pub data: (String, UiReturnDataEncoding),
}

impl Default for UiTransactionReturnData {
    fn default() -> Self {
        Self {
            program_id: String::default(),
            data: (String::default(), UiReturnDataEncoding::Base64),
        }
    }
}

impl From<TransactionReturnData> for UiTransactionReturnData {
    fn from(return_data: TransactionReturnData) -> Self {
        Self {
            program_id: return_data.program_id.to_string(),
            data: (
                BASE64_STANDARD.encode(return_data.data),
                UiReturnDataEncoding::Base64,
            ),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum UiReturnDataEncoding {
    Base64,
}

/// A duplicate representation of LoadedAddresses
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiLoadedAddresses {
    pub writable: Vec<String>,
    pub readonly: Vec<String>,
}

impl From<&LoadedAddresses> for UiLoadedAddresses {
    fn from(loaded_addresses: &LoadedAddresses) -> Self {
        Self {
            writable: loaded_addresses
                .writable
                .iter()
                .map(ToString::to_string)
                .collect(),
            readonly: loaded_addresses
                .readonly
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransactionTokenBalance {
    pub account_index: u8,
    pub mint: String,
    pub ui_token_amount: UiTokenAmount,
    pub owner: String,
    pub program_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiTransactionTokenBalance {
    pub account_index: u8,
    pub mint: String,
    pub ui_token_amount: UiTokenAmount,
    #[serde(
        default = "OptionSerializer::skip",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub owner: OptionSerializer<String>,
    #[serde(
        default = "OptionSerializer::skip",
        skip_serializing_if = "OptionSerializer::should_skip"
    )]
    pub program_id: OptionSerializer<String>,
}

impl From<TransactionTokenBalance> for UiTransactionTokenBalance {
    fn from(token_balance: TransactionTokenBalance) -> Self {
        Self {
            account_index: token_balance.account_index,
            mint: token_balance.mint,
            ui_token_amount: token_balance.ui_token_amount,
            owner: if !token_balance.owner.is_empty() {
                OptionSerializer::Some(token_balance.owner)
            } else {
                OptionSerializer::Skip
            },
            program_id: if !token_balance.program_id.is_empty() {
                OptionSerializer::Some(token_balance.program_id)
            } else {
                OptionSerializer::Skip
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiAccountsList {
    pub signatures: Vec<String>,
    pub account_keys: Vec<ParsedAccount>,
}

/// A duplicate representation of a Message, in raw format, for pretty JSON serialization
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiRawMessage {
    pub header: MessageHeader,
    pub account_keys: Vec<String>,
    pub recent_blockhash: String,
    pub instructions: Vec<UiCompiledInstruction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address_table_lookups: Option<Vec<UiAddressTableLookup>>,
}

/// A duplicate representation of a CompiledInstruction for pretty JSON serialization
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiCompiledInstruction {
    pub program_id_index: u8,
    pub accounts: Vec<u8>,
    pub data: String,
    pub stack_height: Option<u32>,
}

impl UiCompiledInstruction {
    pub fn from(instruction: &CompiledInstruction, stack_height: Option<u32>) -> Self {
        Self {
            program_id_index: instruction.program_id_index,
            accounts: instruction.accounts.clone(),
            data: bs58::encode(&instruction.data).into_string(),
            stack_height,
        }
    }
}

/// A duplicate representation of an Instruction for pretty JSON serialization
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum UiInstruction {
    Compiled(UiCompiledInstruction),
    Parsed(UiParsedInstruction),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum UiParsedInstruction {
    Parsed(ParsedInstruction),
    PartiallyDecoded(UiPartiallyDecodedInstruction),
}

/// A partially decoded CompiledInstruction that includes explicit account addresses
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPartiallyDecodedInstruction {
    pub program_id: String,
    pub accounts: Vec<String>,
    pub data: String,
    pub stack_height: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedInstruction {
    pub program: String,
    pub program_id: String,
    pub parsed: Value,
    pub stack_height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiInnerInstructions {
    /// Transaction instruction index
    pub index: u8,
    /// List of inner instructions
    pub instructions: Vec<UiInstruction>,
}

impl From<InnerInstructions> for UiInnerInstructions {
    fn from(inner_instructions: InnerInstructions) -> Self {
        Self {
            index: inner_instructions.index,
            instructions: inner_instructions
                .instructions
                .iter()
                .map(
                    |InnerInstruction {
                         instruction: ix,
                         stack_height,
                     }| {
                        UiInstruction::Compiled(UiCompiledInstruction::from(ix, *stack_height))
                    },
                )
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InnerInstructions {
    /// Transaction instruction index
    pub index: u8,
    /// List of inner instructions
    pub instructions: Vec<InnerInstruction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InnerInstruction {
    /// Compiled instruction
    pub instruction: CompiledInstruction,
    /// Invocation stack height of the instruction,
    pub stack_height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransactionStatusMeta {
    pub status: TransactionResult<()>,
    pub fee: u64,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    pub inner_instructions: Option<Vec<InnerInstructions>>,
    pub log_messages: Option<Vec<String>>,
    pub pre_token_balances: Option<Vec<TransactionTokenBalance>>,
    pub post_token_balances: Option<Vec<TransactionTokenBalance>>,
    pub rewards: Option<Rewards>,
    pub loaded_addresses: LoadedAddresses,
    pub return_data: Option<TransactionReturnData>,
    pub compute_units_consumed: Option<u64>,
    pub cost_units: Option<u64>,
}

impl Default for TransactionStatusMeta {
    fn default() -> Self {
        Self {
            status: Ok(()),
            fee: 0,
            pre_balances: vec![],
            post_balances: vec![],
            inner_instructions: None,
            log_messages: None,
            pre_token_balances: None,
            post_token_balances: None,
            rewards: None,
            loaded_addresses: LoadedAddresses::default(),
            return_data: None,
            compute_units_consumed: None,
            cost_units: None,
        }
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodedConfirmedBlock {
    pub previous_blockhash: String,
    pub blockhash: String,
    pub parent_slot: u64,
    pub transactions: Vec<EncodedTransactionWithStatusMeta>,
    pub rewards: Rewards,
    pub num_partitions: Option<u64>,
    pub block_time: Option<i64>,
    pub block_height: Option<u64>,
}

impl From<UiConfirmedBlock> for EncodedConfirmedBlock {
    fn from(block: UiConfirmedBlock) -> Self {
        Self {
            previous_blockhash: block.previous_blockhash,
            blockhash: block.blockhash,
            parent_slot: block.parent_slot,
            transactions: block.transactions.unwrap_or_default(),
            rewards: block.rewards.unwrap_or_default(),
            num_partitions: block.num_reward_partitions,
            block_time: block.block_time,
            block_height: block.block_height,
        }
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodedConfirmedTransactionWithStatusMeta {
    pub slot: u64,
    #[serde(flatten)]
    pub transaction: EncodedTransactionWithStatusMeta,
    pub block_time: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionStatus {
    pub slot: u64,
    pub confirmations: Option<usize>,  // None = rooted
    pub status: TransactionResult<()>, // legacy field
    pub err: Option<TransactionError>,
    pub confirmation_status: Option<TransactionConfirmationStatus>,
}

impl TransactionStatus {
    pub fn satisfies_commitment(&self, commitment_config: CommitmentConfig) -> bool {
        if commitment_config.is_finalized() {
            self.confirmations.is_none()
        } else if commitment_config.is_confirmed() {
            if let Some(status) = &self.confirmation_status {
                *status != TransactionConfirmationStatus::Processed
            } else {
                // These fallback cases handle TransactionStatus RPC responses from older software
                self.confirmations.is_some() && self.confirmations.unwrap() > 1
                    || self.confirmations.is_none()
            }
        } else {
            true
        }
    }

    // Returns `confirmation_status`, or if is_none, determines the status from confirmations.
    // Facilitates querying nodes on older software
    pub fn confirmation_status(&self) -> TransactionConfirmationStatus {
        match &self.confirmation_status {
            Some(status) => status.clone(),
            None => {
                if self.confirmations.is_none() {
                    TransactionConfirmationStatus::Finalized
                } else if self.confirmations.unwrap() > 0 {
                    TransactionConfirmationStatus::Confirmed
                } else {
                    TransactionConfirmationStatus::Processed
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        serde_json::{from_value, json, to_value},
        test_case::test_case,
    };

    #[test]
    fn test_decode_invalid_transaction() {
        // This transaction will not pass sanitization
        let unsanitary_transaction = EncodedTransaction::Binary(
            "ju9xZWuDBX4pRxX2oZkTjxU5jB4SSTgEGhX8bQ8PURNzyzqKMPPpNvWihx8zUe\
             FfrbVNoAaEsNKZvGzAnTDy5bhNT9kt6KFCTBixpvrLCzg4M5UdFUQYrn1gdgjX\
             pLHxcaShD81xBNaFDgnA2nkkdHnKtZt4hVSfKAmw3VRZbjrZ7L2fKZBx21CwsG\
             hD6onjM2M3qZW5C8J6d1pj41MxKmZgPBSha3MyKkNLkAGFASK"
                .to_string(),
            TransactionBinaryEncoding::Base58,
        );
        assert!(unsanitary_transaction.decode().is_none());
    }

    #[test]
    fn test_satisfies_commitment() {
        let status = TransactionStatus {
            slot: 0,
            confirmations: None,
            status: Ok(()),
            err: None,
            confirmation_status: Some(TransactionConfirmationStatus::Finalized),
        };

        assert!(status.satisfies_commitment(CommitmentConfig::finalized()));
        assert!(status.satisfies_commitment(CommitmentConfig::confirmed()));
        assert!(status.satisfies_commitment(CommitmentConfig::processed()));

        let status = TransactionStatus {
            slot: 0,
            confirmations: Some(10),
            status: Ok(()),
            err: None,
            confirmation_status: Some(TransactionConfirmationStatus::Confirmed),
        };

        assert!(!status.satisfies_commitment(CommitmentConfig::finalized()));
        assert!(status.satisfies_commitment(CommitmentConfig::confirmed()));
        assert!(status.satisfies_commitment(CommitmentConfig::processed()));

        let status = TransactionStatus {
            slot: 0,
            confirmations: Some(1),
            status: Ok(()),
            err: None,
            confirmation_status: Some(TransactionConfirmationStatus::Processed),
        };

        assert!(!status.satisfies_commitment(CommitmentConfig::finalized()));
        assert!(!status.satisfies_commitment(CommitmentConfig::confirmed()));
        assert!(status.satisfies_commitment(CommitmentConfig::processed()));

        let status = TransactionStatus {
            slot: 0,
            confirmations: Some(0),
            status: Ok(()),
            err: None,
            confirmation_status: None,
        };

        assert!(!status.satisfies_commitment(CommitmentConfig::finalized()));
        assert!(!status.satisfies_commitment(CommitmentConfig::confirmed()));
        assert!(status.satisfies_commitment(CommitmentConfig::processed()));

        // Test single_gossip fallback cases
        let status = TransactionStatus {
            slot: 0,
            confirmations: Some(1),
            status: Ok(()),
            err: None,
            confirmation_status: None,
        };
        assert!(!status.satisfies_commitment(CommitmentConfig::confirmed()));

        let status = TransactionStatus {
            slot: 0,
            confirmations: Some(2),
            status: Ok(()),
            err: None,
            confirmation_status: None,
        };
        assert!(status.satisfies_commitment(CommitmentConfig::confirmed()));

        let status = TransactionStatus {
            slot: 0,
            confirmations: None,
            status: Ok(()),
            err: None,
            confirmation_status: None,
        };
        assert!(status.satisfies_commitment(CommitmentConfig::confirmed()));
    }

    #[test]
    fn test_serde_empty_fields() {
        fn test_serde<'de, T: serde::Serialize + serde::Deserialize<'de>>(
            json_input: &'de str,
            expected_json_output: &str,
        ) {
            let typed_meta: T = serde_json::from_str(json_input).unwrap();
            let reserialized_value = json!(typed_meta);

            let expected_json_output_value: serde_json::Value =
                serde_json::from_str(expected_json_output).unwrap();
            assert_eq!(reserialized_value, expected_json_output_value);
        }

        #[rustfmt::skip]
        let json_input = "{\
            \"err\":null,\
            \"status\":{\"Ok\":null},\
            \"fee\":1234,\
            \"preBalances\":[1,2,3],\
            \"postBalances\":[4,5,6]\
        }";
        #[rustfmt::skip]
        let expected_json_output = "{\
            \"err\":null,\
            \"status\":{\"Ok\":null},\
            \"fee\":1234,\
            \"preBalances\":[1,2,3],\
            \"postBalances\":[4,5,6],\
            \"innerInstructions\":null,\
            \"logMessages\":null,\
            \"preTokenBalances\":null,\
            \"postTokenBalances\":null,\
            \"rewards\":null\
        }";
        test_serde::<UiTransactionStatusMeta>(json_input, expected_json_output);

        #[rustfmt::skip]
        let json_input = "{\
            \"accountIndex\":5,\
            \"mint\":\"DXM2yVSouSg1twmQgHLKoSReqXhtUroehWxrTgPmmfWi\",\
            \"uiTokenAmount\": {
                \"amount\": \"1\",\
                \"decimals\": 0,\
                \"uiAmount\": 1.0,\
                \"uiAmountString\": \"1\"\
            }\
        }";
        #[rustfmt::skip]
        let expected_json_output = "{\
            \"accountIndex\":5,\
            \"mint\":\"DXM2yVSouSg1twmQgHLKoSReqXhtUroehWxrTgPmmfWi\",\
            \"uiTokenAmount\": {
                \"amount\": \"1\",\
                \"decimals\": 0,\
                \"uiAmount\": 1.0,\
                \"uiAmountString\": \"1\"\
            }\
        }";
        test_serde::<UiTransactionTokenBalance>(json_input, expected_json_output);
    }

    #[test_case(
        TransactionError::InstructionError (42, InstructionError::Custom(0xdeadbeef)),
        json!({"InstructionError": [
            42,
            { "Custom": 0xdeadbeef_u32 },
        ]});
        "`InstructionError`"
    )]
    #[test_case(TransactionError::InsufficientFundsForRent {
        account_index: 42,
    }, json!({"InsufficientFundsForRent": {
        "account_index": 42,
    }}); "Struct variant error")]
    #[test_case(TransactionError::DuplicateInstruction(42), json!({ "DuplicateInstruction": 42 }); "Single-value tuple variant error")]
    #[test_case(TransactionError::InsufficientFundsForFee, json!("InsufficientFundsForFee"); "Named variant error")]
    fn test_serialize_ui_transaction_error(
        transaction_error: TransactionError,
        expected_serialization: Value,
    ) {
        let actual_serialization = to_value(UiTransactionError(transaction_error))
            .expect("Failed to serialize `UiTransactionError");
        assert_eq!(actual_serialization, expected_serialization);
    }

    #[test_case(
        TransactionError::InstructionError (42, InstructionError::Custom(0xdeadbeef)),
        json!({"InstructionError": [
            42,
            { "Custom": 0xdeadbeef_u32 },
        ]});
        "`InstructionError`"
    )]
    #[test_case(TransactionError::InsufficientFundsForRent {
        account_index: 42,
    }, json!({"InsufficientFundsForRent": {
        "account_index": 42,
    }}); "Struct variant error")]
    #[test_case(TransactionError::DuplicateInstruction(42), json!({ "DuplicateInstruction": 42 }); "Single-value tuple variant error")]
    #[test_case(TransactionError::InsufficientFundsForFee, json!("InsufficientFundsForFee"); "Named variant error")]
    fn test_deserialize_ui_transaction_error(
        expected_transaction_error: TransactionError,
        serialized_value: Value,
    ) {
        let UiTransactionError(actual_transaction_error) =
            from_value::<UiTransactionError>(serialized_value)
                .expect("Failed to deserialize `UiTransactionError");
        assert_eq!(actual_transaction_error, expected_transaction_error);
    }

    #[test]
    fn test_deserialize_instruction_error_string_format() {
        // Test that we can deserialize new InstructionErrors when serialized as
        // a string.
        let new_error = TransactionError::InstructionError(0, InstructionError::BorshIoError);
        let error_json = to_value(&new_error).unwrap();
        let result = from_value::<UiTransactionError>(error_json);
        assert!(matches!(
            result.unwrap().0,
            TransactionError::InstructionError(0, InstructionError::BorshIoError)
        ));

        // This checks compatibility across SDK versions where BorshIoError is
        // a unit variant in v3 and newtype variant in v2.
        let old_error =
            to_value(serde_json::json!({"InstructionError": [0, {"BorshIoError": "Unknown"}]}))
                .unwrap();
        let result = from_value::<UiTransactionError>(old_error);
        assert!(matches!(
            result.unwrap().0,
            TransactionError::InstructionError(0, InstructionError::BorshIoError)
        ));
    }
}
