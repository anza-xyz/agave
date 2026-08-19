use {
    crate::crds_value::CrdsValue, crossbeam_channel::Sender, solana_clock::Slot,
    solana_transaction::Transaction,
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
    Flush(Sender<()>),
}
