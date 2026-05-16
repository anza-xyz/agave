use {
    solana_pubkey::Pubkey,
    std::{error, fmt},
};

// Must be idempotent: AlreadyInitialized is non-fatal.
pub trait TipProcessor: Send + Sync + 'static {
    fn process(&self, ctx: &TipContext<'_>) -> Result<(), TipProcessorError>;
}

#[non_exhaustive]
pub struct TipContext<'a> {
    pub slot: u64,
    pub epoch: u64,
    pub validator_fee_payer: &'a Pubkey,
}

impl<'a> TipContext<'a> {
    pub fn new(slot: u64, epoch: u64, validator_fee_payer: &'a Pubkey) -> Self {
        Self {
            slot,
            epoch,
            validator_fee_payer,
        }
    }
}

#[derive(Debug)]
pub enum TipProcessorError {
    /// Tip PDAs were already initialized. Callers should treat this as success.
    AlreadyInitialized,
    InitializationFailed {
        slot: u64,
        reason: String,
    },
}

impl fmt::Display for TipProcessorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitialized => formatter.write_str("tip PDAs already initialized"),
            Self::InitializationFailed { slot, reason } => {
                write!(
                    formatter,
                    "tip PDA initialization failed for slot {slot}: {reason}"
                )
            }
        }
    }
}

impl error::Error for TipProcessorError {}
