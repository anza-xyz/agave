use solana_clock::{BankId, Slot};

#[derive(Clone, Debug)]
pub enum SlotNotification {
    OptimisticallyConfirmed(Slot, BankId),
    /// The (Slot, Parent Slot, Bank Id) tuple for the slot frozen
    Frozen((Slot, Slot, BankId)),
    /// The (Slot, Parent Slot, Bank Id) tuple for the root slot
    Root((Slot, Slot, BankId)),
}
