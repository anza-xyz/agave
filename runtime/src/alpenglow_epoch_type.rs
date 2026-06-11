use solana_clock::{Epoch, Slot};

#[derive(Debug)]
pub(crate) enum AlpenglowEpochType {
    /// This is a full tower epoch.
    Tower,
    /// The epoch started in tower and then switched to alpenglow
    MigrationEpoch {
        num_tower_slots: Slot,
        num_ag_slots: Slot,
        migration_epoch: Epoch,
    },
    /// This is a full alpenglow epoch
    Alpenglow { migration_epoch: Epoch },
}
