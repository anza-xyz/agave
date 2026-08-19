use {
    crate::{crds_value::CrdsValue, duplicate_shred::DuplicateShred},
    crossbeam_channel::Sender,
    solana_clock::Slot,
    solana_keypair::Keypair,
    solana_transaction::Transaction,
    std::sync::Arc,
};

/// Domain operations submitted by local validator services to the gossip
/// engine. Stateful label selection stays on the engine thread.
pub(crate) enum GossipCommand {
    Publish(CrdsValue),
    LowestSlot(Slot),
    EpochSlots {
        slots: Vec<Slot>,
        completed: Sender<()>,
    },
    RefreshContact(Sender<()>),
    Vote {
        slot: Slot,
        transaction: Transaction,
        completed: Sender<Result<(), Transaction>>,
    },
    RefreshVote {
        transaction: Transaction,
        slot: Slot,
        completed: Sender<()>,
    },
    DuplicateShred {
        keypair: Arc<Keypair>,
        chunks: Vec<DuplicateShred>,
        completed: Sender<()>,
    },
    Flush(Sender<()>),
}
