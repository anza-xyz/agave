use {
    crate::filter::RpcFilterType,
    serde::{Deserialize, Serialize},
    solana_clock::{Epoch, Slot},
};
pub use {
    solana_account_decoder_client_types::{UiAccountEncoding, UiDataSliceConfig},
    solana_commitment_config::{CommitmentConfig, CommitmentLevel},
    solana_transaction_status_client_types::{TransactionDetails, UiTransactionEncoding},
};

// Flattening Option<CommitmentConfig> treats invalid commitment values as missing.
// Read the level directly while preserving the public field type and JSON format.
mod option_commitment_config {
    use {
        serde::{Deserialize, Deserializer, Serialize, Serializer},
        solana_commitment_config::{CommitmentConfig, CommitmentLevel},
    };

    pub fn serialize<S: Serializer>(
        commitment: &Option<CommitmentConfig>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        commitment
            .map(|config| config.commitment)
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<CommitmentConfig>, D::Error> {
        Option::<CommitmentLevel>::deserialize(deserializer)
            .map(|commitment| commitment.map(|commitment| CommitmentConfig { commitment }))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSignatureStatusConfig {
    #[serde(default)]
    pub search_transaction_history: bool,
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub min_context_slot: Option<Slot>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSendTransactionConfig {
    #[serde(default)]
    pub skip_preflight: bool,
    pub preflight_commitment: Option<CommitmentLevel>,
    pub encoding: Option<UiTransactionEncoding>,
    pub max_retries: Option<usize>,
    pub min_context_slot: Option<Slot>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSimulateTransactionAccountsConfig {
    pub encoding: Option<UiAccountEncoding>,
    pub addresses: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSimulateTransactionConfig {
    #[serde(default)]
    pub sig_verify: bool,
    #[serde(default)]
    pub replace_recent_blockhash: bool,
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub encoding: Option<UiTransactionEncoding>,
    pub accounts: Option<RpcSimulateTransactionAccountsConfig>,
    pub min_context_slot: Option<Slot>,
    #[serde(default)]
    pub inner_instructions: bool,
    pub inner_instructions_encoding: Option<UiTransactionEncoding>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRequestAirdropConfig {
    pub recent_blockhash: Option<String>, // base-58 encoded blockhash
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcLeaderScheduleConfig {
    pub identity: Option<String>, // validator identity, as a base-58 encoded string
    pub key_by_vote_account: Option<bool>, // key the schedule by vote account instead of identity
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBlockProductionConfigRange {
    pub first_slot: Slot,
    pub last_slot: Option<Slot>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBlockProductionConfig {
    pub identity: Option<String>, // validator identity, as a base-58 encoded string
    pub range: Option<RpcBlockProductionConfigRange>, // current epoch if `None`
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcGetVoteAccountsConfig {
    pub vote_pubkey: Option<String>, // validator vote address, as a base-58 encoded string
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub keep_unstaked_delinquents: Option<bool>,
    pub delinquent_slot_distance: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcLeaderScheduleConfigWrapper {
    SlotOnly(Option<Slot>),
    ConfigOnly(Option<RpcLeaderScheduleConfig>),
}

impl RpcLeaderScheduleConfigWrapper {
    pub fn unzip(&self) -> (Option<Slot>, Option<RpcLeaderScheduleConfig>) {
        match &self {
            RpcLeaderScheduleConfigWrapper::SlotOnly(slot) => (*slot, None),
            RpcLeaderScheduleConfigWrapper::ConfigOnly(config) => (None, config.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcLargestAccountsFilter {
    Circulating,
    NonCirculating,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcLargestAccountsConfig {
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub filter: Option<RpcLargestAccountsFilter>,
    pub sort_results: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSupplyConfig {
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    #[serde(default)]
    pub exclude_non_circulating_accounts_list: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcEpochConfig {
    pub epoch: Option<Epoch>,
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub min_context_slot: Option<Slot>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum RpcAccountIndex {
    ProgramId,
    SplTokenMint,
    SplTokenOwner,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcAccountInfoConfig {
    pub encoding: Option<UiAccountEncoding>,
    pub data_slice: Option<UiDataSliceConfig>,
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub min_context_slot: Option<Slot>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcProgramAccountsConfig {
    pub filters: Option<Vec<RpcFilterType>>,
    #[serde(flatten)]
    pub account_config: RpcAccountInfoConfig,
    pub with_context: Option<bool>,
    pub sort_results: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcTransactionLogsFilter {
    All,
    /// Deprecated compatibility alias for [`Self::All`].
    #[deprecated(
        since = "4.5.0",
        note = "Alpenglow has no vote transactions; use `All` instead"
    )]
    AllWithVotes,
    Mentions(Vec<String>), // base58-encoded list of addresses
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcTransactionLogsConfig {
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcTokenAccountsFilter {
    Mint(String),
    ProgramId(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSignatureSubscribeConfig {
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub enable_received_notification: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcBlockSubscribeFilter {
    All,
    MentionsAccountOrProgram(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBlockSubscribeConfig {
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub encoding: Option<UiTransactionEncoding>,
    pub transaction_details: Option<TransactionDetails>,
    pub show_rewards: Option<bool>,
    pub max_supported_transaction_version: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSignaturesForAddressConfig {
    pub before: Option<String>, // Signature as base-58 string
    pub until: Option<String>,  // Signature as base-58 string
    pub limit: Option<usize>,
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub min_context_slot: Option<Slot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcEncodingConfigWrapper<T> {
    Deprecated(Option<UiTransactionEncoding>),
    Current(Option<T>),
}

impl<T: EncodingConfig + Default + Copy> RpcEncodingConfigWrapper<T> {
    pub fn convert_to_current(&self) -> T {
        match self {
            RpcEncodingConfigWrapper::Deprecated(encoding) => T::new_with_encoding(encoding),
            RpcEncodingConfigWrapper::Current(config) => config.unwrap_or_default(),
        }
    }

    pub fn convert<U: EncodingConfig + From<T>>(&self) -> RpcEncodingConfigWrapper<U> {
        match self {
            RpcEncodingConfigWrapper::Deprecated(encoding) => {
                RpcEncodingConfigWrapper::Deprecated(*encoding)
            }
            RpcEncodingConfigWrapper::Current(config) => {
                RpcEncodingConfigWrapper::Current(config.map(|config| config.into()))
            }
        }
    }
}

pub trait EncodingConfig {
    fn new_with_encoding(encoding: &Option<UiTransactionEncoding>) -> Self;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBlockConfig {
    pub encoding: Option<UiTransactionEncoding>,
    pub transaction_details: Option<TransactionDetails>,
    pub rewards: Option<bool>,
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub max_supported_transaction_version: Option<u8>,
}

impl EncodingConfig for RpcBlockConfig {
    fn new_with_encoding(encoding: &Option<UiTransactionEncoding>) -> Self {
        Self {
            encoding: *encoding,
            ..Self::default()
        }
    }
}

impl RpcBlockConfig {
    pub fn rewards_only() -> Self {
        Self {
            transaction_details: Some(TransactionDetails::None),
            ..Self::default()
        }
    }

    pub fn rewards_with_commitment(commitment: Option<CommitmentConfig>) -> Self {
        Self {
            transaction_details: Some(TransactionDetails::None),
            commitment,
            ..Self::default()
        }
    }
}

impl From<RpcBlockConfig> for RpcEncodingConfigWrapper<RpcBlockConfig> {
    fn from(config: RpcBlockConfig) -> Self {
        RpcEncodingConfigWrapper::Current(Some(config))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcTransactionConfig {
    pub encoding: Option<UiTransactionEncoding>,
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub max_supported_transaction_version: Option<u8>,
    pub min_context_slot: Option<Slot>,
}

impl EncodingConfig for RpcTransactionConfig {
    fn new_with_encoding(encoding: &Option<UiTransactionEncoding>) -> Self {
        Self {
            encoding: *encoding,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcBlocksConfigWrapper {
    EndSlotOnly(Option<Slot>),
    ConfigOnly(Option<RpcContextConfig>),
}

impl RpcBlocksConfigWrapper {
    pub fn unzip(&self) -> (Option<Slot>, Option<RpcContextConfig>) {
        match &self {
            RpcBlocksConfigWrapper::EndSlotOnly(end_slot) => (*end_slot, None),
            RpcBlocksConfigWrapper::ConfigOnly(config) => (None, *config),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcContextConfig {
    #[serde(
        default,
        with = "option_commitment_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub commitment: Option<CommitmentConfig>,
    pub min_context_slot: Option<Slot>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcRankMapConfig {
    /// Filter by validator identity, as a base-58 encoded string.
    pub identity: Option<String>,
    /// Minimum finalized bank context slot; no minimum is enforced when omitted.
    pub min_context_slot: Option<Slot>,
}

#[cfg(test)]
mod tests {
    use {super::*, serde::de::DeserializeOwned, serde_json::json, std::fmt::Debug};

    fn check_commitment_config<T: DeserializeOwned + Serialize + Debug>() {
        for commitment in ["processed", "confirmed", "finalized"] {
            let config: T = serde_json::from_value(json!({"commitment": commitment})).unwrap();
            assert_eq!(
                serde_json::to_value(config).unwrap()["commitment"],
                commitment
            );
        }

        for input in [json!({}), json!({"commitment": null})] {
            let config: T = serde_json::from_value(input).unwrap();
            assert!(
                serde_json::to_value(config)
                    .unwrap()
                    .get("commitment")
                    .is_none()
            );
        }

        for commitment in [
            json!("bad"),
            json!(""),
            json!(123),
            json!(true),
            json!([]),
            json!({}),
        ] {
            let error = serde_json::from_value::<T>(json!({"commitment": commitment}))
                .expect_err("invalid commitment must be rejected");
            assert!(error.is_data(), "{error}");
        }
    }

    #[test]
    fn test_commitment_config_deserialization() {
        check_commitment_config::<RpcSignatureStatusConfig>();
        check_commitment_config::<RpcSimulateTransactionConfig>();
        check_commitment_config::<RpcRequestAirdropConfig>();
        check_commitment_config::<RpcLeaderScheduleConfig>();
        check_commitment_config::<RpcBlockProductionConfig>();
        check_commitment_config::<RpcGetVoteAccountsConfig>();
        check_commitment_config::<RpcLargestAccountsConfig>();
        check_commitment_config::<RpcSupplyConfig>();
        check_commitment_config::<RpcEpochConfig>();
        check_commitment_config::<RpcAccountInfoConfig>();
        check_commitment_config::<RpcProgramAccountsConfig>();
        check_commitment_config::<RpcTransactionLogsConfig>();
        check_commitment_config::<RpcSignatureSubscribeConfig>();
        check_commitment_config::<RpcBlockSubscribeConfig>();
        check_commitment_config::<RpcSignaturesForAddressConfig>();
        check_commitment_config::<RpcBlockConfig>();
        check_commitment_config::<RpcTransactionConfig>();
        check_commitment_config::<RpcContextConfig>();
        check_commitment_config::<RpcBlocksConfigWrapper>();
        check_commitment_config::<RpcLeaderScheduleConfigWrapper>();
        check_commitment_config::<RpcEncodingConfigWrapper<RpcBlockConfig>>();
        check_commitment_config::<RpcEncodingConfigWrapper<RpcTransactionConfig>>();
    }

    #[test]
    fn test_commitment_config_preserves_other_fields() {
        let config: RpcProgramAccountsConfig = serde_json::from_value(json!({
            "commitment": "confirmed",
            "encoding": "base64",
            "minContextSlot": 123,
            "withContext": true,
            "unknownField": "ignored",
        }))
        .unwrap();
        assert_eq!(
            config.account_config.commitment,
            Some(CommitmentConfig::confirmed())
        );
        assert_eq!(
            config.account_config.encoding,
            Some(UiAccountEncoding::Base64)
        );
        assert_eq!(config.account_config.min_context_slot, Some(123));
        assert_eq!(config.with_context, Some(true));
    }

    #[test]
    #[allow(deprecated)]
    fn test_all_with_votes_wire_compatibility() {
        let filter: RpcTransactionLogsFilter = serde_json::from_str(r#""allWithVotes""#).unwrap();
        assert_eq!(filter, RpcTransactionLogsFilter::AllWithVotes);
        assert_eq!(serde_json::to_string(&filter).unwrap(), r#""allWithVotes""#);
    }
}
