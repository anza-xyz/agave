use {serde::Deserialize, solana_account::ReadableAccount, solana_runtime::bank::Bank};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeConstant {
    /// IIR time-constant (ms)
    Value(u64),
    /// Use the default time constant.
    Default,
}

#[derive(Deserialize, Debug)]
#[repr(C)]
pub struct WeightingConfig {
    pub weighting_mode: u8,
    pub tc_ms: u64,
}

pub const WEIGHTING_MODE_STATIC: u8 = 0;
pub const WEIGHTING_MODE_DYNAMIC: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WeightingConfigTyped {
    Static,
    Dynamic { tc: TimeConstant },
}

impl From<&WeightingConfig> for WeightingConfigTyped {
    fn from(raw: &WeightingConfig) -> Self {
        match raw.weighting_mode {
            WEIGHTING_MODE_STATIC => WeightingConfigTyped::Static,
            WEIGHTING_MODE_DYNAMIC => {
                let tc = if raw.tc_ms == 0 {
                    TimeConstant::Default
                } else {
                    TimeConstant::Value(raw.tc_ms)
                };
                WeightingConfigTyped::Dynamic { tc }
            }
            _ => WeightingConfigTyped::Static,
        }
    }
}

mod weighting_config_control_pubkey {
    solana_pubkey::declare_id!("gosWyfqaAmMhdrSM9srCsV6DihYg7Ze56SvJLwZbNSP");
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
