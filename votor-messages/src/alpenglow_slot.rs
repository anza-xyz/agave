//! Defines AlpenglowSlot

use solana_clock::Slot;

use crate::certificate::GenesisCert;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
/// A struct containing a slot that is validated to be > AG's migration slot.
pub struct AlpenglowSlot(Slot);

impl AlpenglowSlot {
    /// Creates a AlpenglowSlot from `slot` if it is > than the migration slot.
    pub fn try_new(slot: Slot, cert: &GenesisCert) -> Option<Self> {
        (slot > cert.block.slot).then_some(Self(slot))
    }

    /// Returns the underlying slot.
    pub fn into(self) -> Slot {
        self.0
    }

    #[cfg(feature = "dev-context-only-utils")]
    /// Creates a new AlpenglowSlot for test purposes.
    pub fn new_for_tests(slot: Slot) -> Self {
        Self(slot)
    }
}
