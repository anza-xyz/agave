use {
    super::{repair_handler::RepairHandler, repair_response},
    solana_clock::Slot,
    solana_ledger::{blockstore::Blockstore, shred::Nonce},
    solana_perf::packet::{Packet, PacketBatch, PacketBatchRecycler, RecycledPacketBatch},
    std::{net::SocketAddr, sync::Arc},
};

pub(crate) struct StandardRepairHandler {
    blockstore: Arc<Blockstore>,
}

impl StandardRepairHandler {
    pub(crate) fn new(blockstore: Arc<Blockstore>) -> Self {
        Self { blockstore }
    }
}

impl RepairHandler for StandardRepairHandler {
    fn blockstore(&self) -> &Blockstore {
        &self.blockstore
    }

    fn repair_response_packet(
        &self,
        slot: Slot,
        shred_index: u64,
        dest: &SocketAddr,
        nonce: Nonce,
    ) -> Option<Packet> {
        repair_response::repair_response_packet(
            self.blockstore.as_ref(),
            slot,
            shred_index,
            dest,
            nonce,
        )
    }

    fn run_orphan(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        max_responses: usize,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        let mut res = RecycledPacketBatch::new_with_recycler(recycler, max_responses, "run_orphan");
        // Try to find the next "n" parent slots of the input slot
        let packets = std::iter::successors(self.blockstore.meta(slot).ok()?, |meta| {
            self.blockstore.meta(meta.parent_slot?).ok()?
        })
        .map_while(|meta| {
            repair_response::repair_response_packet(
                self.blockstore.as_ref(),
                meta.slot,
                meta.received.checked_sub(1u64)?,
                from_addr,
                nonce,
            )
        });
        for packet in packets.take(max_responses) {
            res.push(packet);
        }
        (!res.is_empty()).then_some(res.into())
    }
}
