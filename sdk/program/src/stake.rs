#[deprecated(since = "2.2.0", note = "Use solana-stake-interface instead")]
pub use solana_stake_interface::{
    config, stake_flags, state, tools, MINIMUM_DELINQUENT_EPOCHS_FOR_DEACTIVATION,
};

pub mod instruction {
    #[deprecated(since = "2.2.0", note = "Use solana-stake-interface instead")]
    pub use solana_stake_interface::{error::StakeError, instruction::*};
}

pub mod program {
    pub use solana_sdk_ids::stake::{check_id, id, ID};
}
