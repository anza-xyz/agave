//! Context for a slot.

use crate::proto::SlotContext as ProtoSlotContext;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SlotContext {
    /// The slot to use for the simulation.
    pub slot: u64,
}

impl From<ProtoSlotContext> for SlotContext {
    fn from(value: ProtoSlotContext) -> Self {
        let ProtoSlotContext { slot } = value;
        Self { slot }
    }
}

impl From<SlotContext> for ProtoSlotContext {
    fn from(value: SlotContext) -> Self {
        let SlotContext { slot } = value;
        Self { slot }
    }
}
