use {
    crate::{crds_value::CrdsValue, duplicate_shred::DuplicateShred},
    crossbeam_channel::Sender,
    solana_clock::Slot,
    solana_keypair::Keypair,
    solana_transaction::Transaction,
    std::sync::Arc,
};

/// Gossip mutations serialized by the engine.
// Keep frequent votes inline to avoid a per-vote allocation.
#[allow(clippy::large_enum_variant)]
pub(crate) enum GossipCommand {
    Publish(Box<CrdsValue>),
    LowestSlot(Slot),
    EpochSlots(Vec<Slot>),
    RefreshContact(Sender<()>),
    Vote {
        slot: Slot,
        transaction: Transaction,
        completed: Sender<Result<(), Transaction>>,
    },
    RefreshVote {
        transaction: Transaction,
        slot: Slot,
    },
    DuplicateShred {
        keypair: Arc<Keypair>,
        chunks: Vec<DuplicateShred>,
    },
    Flush(Sender<()>),
}

pub(crate) const GOSSIP_COMMAND_CAPACITY: usize = 256;
