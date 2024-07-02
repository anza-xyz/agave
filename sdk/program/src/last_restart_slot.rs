//! Information about the last restart slot (hard fork).

use {crate::clock::Slot, solana_sdk_macro::NoPadding};

#[repr(C)]
#[derive(Serialize, Deserialize, Debug, NoPadding, PartialEq, Eq, Default, Clone)]
pub struct LastRestartSlot {
    /// The last restart `Slot`.
    pub last_restart_slot: Slot,
}
