pub const MAX_SHORT_FIELD_LENGTH: usize = 80;
pub const MAX_LONG_FIELD_LENGTH: usize = 300;
/// Maximum size of validator configuration data (`ValidatorInfo`).
pub const MAX_VALIDATOR_INFO: u64 = 576;

solana_pubkey::declare_id!("Va1idator1nfo111111111111111111111111111111");

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize, Default)]
pub struct ValidatorInfo {
    pub info: String,
}
