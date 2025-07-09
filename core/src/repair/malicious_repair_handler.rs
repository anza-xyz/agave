use {
    super::{
        repair_handler::RepairHandler, repair_response::repair_response_packet_from_bytes,
        standard_repair_handler::StandardRepairHandler,
    },
    solana_clock::Slot,
    solana_ledger::{
        blockstore::Blockstore,
        shred::{Nonce, SIZE_OF_DATA_SHRED_HEADERS},
    },
    solana_perf::packet::{Packet, PacketBatch, PacketBatchRecycler, PinnedPacketBatch},
    std::{net::SocketAddr, sync::Arc},
};

#[derive(Copy, Clone, Debug, Default)]
pub struct MaliciousRepairConfig {
    bad_shred_slot_frequency: Option<Slot>,
}

pub struct MaliciousRepairHandler {
    blockstore: Arc<Blockstore>,
    standard_handler: StandardRepairHandler,
    config: MaliciousRepairConfig,
}

impl MaliciousRepairHandler {
    const BAD_DATA_INDEX: usize = SIZE_OF_DATA_SHRED_HEADERS + 5;

    pub fn new(blockstore: Arc<Blockstore>, config: MaliciousRepairConfig) -> Self {
        let standard_handler = StandardRepairHandler::new(blockstore.clone());
        Self {
            blockstore,
            standard_handler,
            config,
        }
    }

    fn repair_response_packet(
        &self,
        slot: Slot,
        shred_index: u64,
        dest: &SocketAddr,
        nonce: Nonce,
    ) -> Option<Packet> {
        let mut shred = self
            .blockstore
            .get_data_shred(slot, shred_index)
            .expect("Blockstore could not get data shred")?;
        if self
            .config
            .bad_shred_slot_frequency
            .is_some_and(|freq| slot % freq == 0)
        {
            // Change some random piece of data
            shred[Self::BAD_DATA_INDEX] = shred[Self::BAD_DATA_INDEX].wrapping_add(1);
        }
        repair_response_packet_from_bytes(shred, dest, nonce)
    }
}

impl RepairHandler for MaliciousRepairHandler {
    fn run_window_request(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        shred_index: u64,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        let packet = self.repair_response_packet(slot, shred_index, from_addr, nonce)?;
        Some(
            PinnedPacketBatch::new_unpinned_with_recycler_data(
                recycler,
                "run_window_request",
                vec![packet],
            )
            .into(),
        )
    }

    fn run_highest_window_request(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        highest_index: u64,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        // Try to find the requested index in one of the slots
        let meta = self.blockstore.meta(slot).ok()??;
        if meta.received > highest_index {
            // meta.received must be at least 1 by this point
            let packet = self.repair_response_packet(slot, meta.received - 1, from_addr, nonce)?;
            return Some(
                PinnedPacketBatch::new_unpinned_with_recycler_data(
                    recycler,
                    "run_highest_window_request",
                    vec![packet],
                )
                .into(),
            );
        }
        None
    }

    fn run_orphan(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        max_responses: usize,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        self.standard_handler
            .run_orphan(recycler, from_addr, slot, max_responses, nonce)
    }

    fn run_ancestor_hashes(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        self.standard_handler
            .run_ancestor_hashes(recycler, from_addr, slot, nonce)
    }
}
