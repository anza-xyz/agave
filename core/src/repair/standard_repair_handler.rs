use {
    super::{repair_handler::RepairHandler, repair_response},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::{
        blockstore::{Blockstore, SlotMeta},
        blockstore_meta::BlockLocation,
        shred::Nonce,
    },
    solana_perf::packet::{Packet, PacketBatch, PacketBatchRecycler, PinnedPacketBatch},
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
        block_id: Option<Hash>,
        dest: &SocketAddr,
        nonce: Nonce,
    ) -> Option<Packet> {
        match block_id {
            None => repair_response::repair_response_packet(
                self.blockstore.as_ref(),
                slot,
                shred_index,
                dest,
                nonce,
            ),
            Some(block_id) => {
                let location = self
                    .blockstore()
                    .get_block_location(slot, block_id)
                    .expect("Unable to fetch block location from blockstore")?;
                let shred = self
                    .blockstore()
                    .get_data_shred_from_location(slot, shred_index, location)
                    .expect("Blockstore could not get data shred")?;
                repair_response::repair_response_packet_from_bytes(shred, dest, nonce)
            }
        }
    }

    fn run_orphan(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        block_id: Option<Hash>,
        max_responses: usize,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        match block_id {
            None => self.run_orphan_turbine_only(recycler, from_addr, slot, max_responses, nonce),
            Some(block_id) => self.run_orphan_for_block_id(
                recycler,
                from_addr,
                slot,
                block_id,
                max_responses,
                nonce,
            ),
        }
    }
}

impl StandardRepairHandler {
    /// Fulfills orphan requests for the specified `block_id`.
    /// We responded with up to `max_repsonses` ancestors, only if we've fully ingested said blocks.
    fn run_orphan_for_block_id(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        block_id: Hash,
        max_responses: usize,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        let mut res =
            PinnedPacketBatch::new_unpinned_with_recycler(recycler, max_responses, "run_orphan");

        let get_parent_location_meta = |(location, meta): &(BlockLocation, SlotMeta)| {
            let parent_slot = meta.parent_slot?;
            let parent_block_id = self
                .blockstore
                .get_parent_block_id_from_location(meta.slot, *location)
                .ok()??;
            let parent_location = self
                .blockstore
                .get_block_location(parent_slot, parent_block_id)
                .ok()??;
            let parent_meta = self
                .blockstore
                .meta_from_location(parent_slot, parent_location)
                .ok()??;
            Some((parent_location, parent_meta))
        };

        let location = self.blockstore.get_block_location(slot, block_id).ok()??;
        let meta = self.blockstore.meta_from_location(slot, location).ok()??;
        let packets = std::iter::successors(Some((location, meta)), get_parent_location_meta)
            .map_while(|(location, meta)| {
                assert!(meta.is_full());
                let shred = self
                    .blockstore()
                    .get_data_shred_from_location(
                        slot,
                        meta.last_index.expect("slot is full").checked_sub(1u64)?,
                        location,
                    )
                    .ok()??;
                repair_response::repair_response_packet_from_bytes(shred, from_addr, nonce)
            });

        for packet in packets.take(max_responses) {
            res.push(packet);
        }
        (!res.is_empty()).then_some(res.into())
    }

    /// Fulfills orphan requests from the turbine column only. This predates block id logic and is left for compatability
    /// with TowerBFT. This variant will respond to requests even if the block has not been fully ingested.
    fn run_orphan_turbine_only(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        max_responses: usize,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        let mut res =
            PinnedPacketBatch::new_unpinned_with_recycler(recycler, max_responses, "run_orphan");
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
