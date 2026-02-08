use {
    serde::{Deserialize, Serialize},
    solana_fee_calculator::FeeCalculator,
    solana_instruction::error::InstructionError,
    thiserror::Error,
};

pub type StringAmount = String;
pub type StringDecimals = String;

#[derive(Error, Debug)]
pub enum ParseAccountError {
    #[error("{0:?} account not parsable")]
    AccountNotParsable(ParsableAccount),

    #[error("Program not parsable")]
    ProgramNotParsable,

    #[error("Additional data required to parse: {0}")]
    AdditionalDataMissing(String),

    #[error("Instruction error")]
    InstructionError(#[from] InstructionError),

    #[error("Serde json error")]
    SerdeJsonError(#[from] serde_json::error::Error),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParsableAccount {
    AddressLookupTable,
    BpfUpgradeableLoader,
    Config,
    Nonce,
    SplToken,
    SplToken2022,
    Stake,
    Sysvar,
    Vote,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UiFeeCalculator {
    pub lamports_per_signature: StringAmount,
}

impl From<FeeCalculator> for UiFeeCalculator {
    fn from(fee_calculator: FeeCalculator) -> Self {
        Self {
            lamports_per_signature: fee_calculator.lamports_per_signature.to_string(),
        }
    }
}

impl Default for UiFeeCalculator {
    fn default() -> Self {
        Self {
            lamports_per_signature: "0".to_string(),
        }
    }
}

#[cfg(feature = "token")]
use {
    solana_clock::UnixTimestamp,
    spl_token_2022_interface::extension::{
        interest_bearing_mint::InterestBearingConfig, scaled_ui_amount::ScaledUiAmountConfig,
    },
};

#[cfg(feature = "token")]
#[derive(Clone, Copy, Default)]
pub struct SplTokenAdditionalDataV2 {
    pub decimals: u8,
    pub interest_bearing_config: Option<(InterestBearingConfig, UnixTimestamp)>,
    pub scaled_ui_amount_config: Option<(ScaledUiAmountConfig, UnixTimestamp)>,
}

#[cfg(feature = "token")]
impl SplTokenAdditionalDataV2 {
    pub fn with_decimals(decimals: u8) -> Self {
        Self {
            decimals,
            ..Default::default()
        }
    }
}
