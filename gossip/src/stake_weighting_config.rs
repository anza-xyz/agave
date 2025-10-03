use {
    serde::{Deserialize, Serialize},
    solana_account::ReadableAccount,
    solana_runtime::bank::Bank,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TimeConstant {
    /// IIR time-constant (ms)
    Value(u64),
    /// Use the default time constant.
    Default,
}

/// Actual on-chain state that controls the weighting of gossip nodes
#[derive(Serialize, Deserialize, Debug, Default)]
#[repr(C)]
pub(crate) struct WeightingConfig {
    _version: u8,           // This is part of Record program header
    _authority: [u8; 32],   // This is part of Record program header
    pub weighting_mode: u8, // 0 = Static, 1 = Dynamic
    pub tc_ms: u64,         // IIR time constant in milliseconds
    _future_use: [u8; 16],  // Reserved for future use
}

pub const WEIGHTING_MODE_STATIC: u8 = 0;
pub const WEIGHTING_MODE_DYNAMIC: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum WeightingConfigTyped {
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

impl WeightingConfig {
    #[cfg(test)]
    pub(crate) fn new_for_test(weighting_mode: u8, tc_ms: u64) -> Self {
        Self {
            _version: 0,
            _authority: [0; 32],
            weighting_mode,
            tc_ms,
            _future_use: [0; 16],
        }
    }
}

mod weighting_config_control_pubkey {
    solana_pubkey::declare_id!("noDynamicGossipWeights111111111111111111111");
}

pub(crate) fn get_gossip_config_from_account(bank: &Bank) -> Option<WeightingConfig> {
    let data = bank
        .accounts()
        .accounts_db
        .load_account_with(
            &bank.ancestors,
            &weighting_config_control_pubkey::id(),
            true,
        )?
        .0;
    bincode::deserialize::<WeightingConfig>(data.data()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_pubkey_is_offcurve() {
        assert!(
            !weighting_config_control_pubkey::id().is_on_curve(),
            "weighting_config_control_pubkey must be an off-curve key"
        );
    }
}
