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
///
/// Only commands whose caller needs the outcome, or needs to know the command
/// has been applied, carry a `completed` channel. Ordering between commands is
/// already guaranteed by the channel.
// Keep transactions inline: votes are frequent enough that a smaller enum is
// not worth an extra heap allocation for every vote.
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
