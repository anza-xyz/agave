use {
    serde::{Deserialize, Serialize},
    solana_account::ReadableAccount,
    solana_runtime::bank::Bank,
};

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[repr(C)]
pub struct WeightingConfig {
    /// 0 = Static, 1 = Dynamic
    pub weighting_mode: u8,
    // IIR time-constant (ms)
    pub tc_ms: u64,
}

mod weighting_config_control_pubkey {
    solana_pubkey::declare_id!("maCwV5aXW6srR4kppBWTeNdizYhyEwVcVh6mSWnFZxo");
}

pub(crate) fn get_gossip_config_from_account(bank: &Bank) -> Option<WeightingConfig> {
    let data = bank
        .accounts()
        .accounts_db
        .load_account_with(
            &bank.ancestors,
            &weighting_config_control_pubkey::id(),
            |_| true,
        )?
        .0;
    bincode::deserialize::<WeightingConfig>(data.data()).ok()
}
