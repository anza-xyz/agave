//! Slot context for transaction execution.

/// Slot-scoped context for transaction execution.
#[derive(Clone, Debug, Default)]
pub struct SlotContext {
    pub slot: u64,
    pub block_height: u64,
    pub prev_slot: u64,
    pub prev_lamports_per_signature: u64,
}

#[cfg(feature = "fuzz")]
use super::proto::SlotContext as ProtoSlotContext;

#[cfg(feature = "fuzz")]
impl From<&ProtoSlotContext> for SlotContext {
    fn from(value: &ProtoSlotContext) -> Self {
        Self {
            slot: value.slot,
            block_height: value.block_height,
            prev_slot: value.prev_slot,
            prev_lamports_per_signature: value.prev_lps,
        }
    }
}
