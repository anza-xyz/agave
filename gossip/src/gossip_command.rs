use {crate::crds_value::CrdsValue, crossbeam_channel::Sender, solana_clock::Slot};

/// Domain operations submitted by local validator services to the gossip
/// engine. Stateful label selection stays on the engine thread.
pub(crate) enum GossipCommand {
    Publish(CrdsValue),
    LowestSlot(Slot),
    EpochSlots {
        slots: Vec<Slot>,
        completed: Sender<()>,
    },
    Flush(Sender<()>),
}
