use {
    super::{
        malicious_repair_handler::{MaliciousRepairConfig, MaliciousRepairHandler},
        serve_repair::ServeRepair,
        standard_repair_handler::StandardRepairHandler,
    },
    crate::repair::{
        repair_response,
        serve_repair::{AncestorHashesResponse, MAX_ANCESTOR_RESPONSES},
    },
    bincode::serialize,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{
        ancestor_iterator::{AncestorIterator, AncestorIteratorWithHash},
        blockstore::Blockstore,
        shred::Nonce,
    },
    solana_perf::packet::{Packet, PacketBatch, PacketBatchRecycler, RecycledPacketBatch},
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        collections::HashSet,
        net::SocketAddr,
        sync::{Arc, RwLock},
    },
};

pub trait RepairHandler {
    fn blockstore(&self) -> &Blockstore;

    fn repair_response_packet(
        &self,
        slot: Slot,
        shred_index: u64,
        dest: &SocketAddr,
        nonce: Nonce,
    ) -> Option<Packet>;

    fn run_window_request(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        shred_index: u64,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        // Try to find the requested index in one of the slots
        let packet = self.repair_response_packet(slot, shred_index, from_addr, nonce)?;
        Some(
            RecycledPacketBatch::new_with_recycler_data(
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
        let meta = self.blockstore().meta(slot).ok()??;
        if meta.received > highest_index {
            // meta.received must be at least 1 by this point
            let packet = self.repair_response_packet(slot, meta.received - 1, from_addr, nonce)?;
            return Some(
                RecycledPacketBatch::new_with_recycler_data(
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
    ) -> Option<PacketBatch>;

    fn run_ancestor_hashes(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        let ancestor_slot_hashes = if self.blockstore().is_duplicate_confirmed(slot) {
            let ancestor_iterator = AncestorIteratorWithHash::from(
                AncestorIterator::new_inclusive(slot, self.blockstore()),
            );
            ancestor_iterator.take(MAX_ANCESTOR_RESPONSES).collect()
        } else {
            // If this slot is not duplicate confirmed, return nothing
            vec![]
        };
        let response = AncestorHashesResponse::Hashes(ancestor_slot_hashes);
        let serialized_response = serialize(&response).ok()?;

        // Could probably directly write response into packet via `serialize_into()`
        // instead of incurring extra copy in `repair_response_packet_from_bytes`, but
        // serialize_into doesn't return the written size...
        let packet = repair_response::repair_response_packet_from_bytes(
            serialized_response,
            from_addr,
            nonce,
        )?;
        Some(
            RecycledPacketBatch::new_with_recycler_data(
                recycler,
                "run_ancestor_hashes",
                vec![packet],
            )
            .into(),
        )
    }
}

#[derive(Clone, Debug, Default)]
pub enum RepairHandlerType {
    #[default]
    Standard,
    Malicious(MaliciousRepairConfig),
}

impl RepairHandlerType {
    pub fn to_handler(&self, blockstore: Arc<Blockstore>) -> Box<dyn RepairHandler + Send + Sync> {
        match self {
            RepairHandlerType::Standard => Box::new(StandardRepairHandler::new(blockstore)),
            RepairHandlerType::Malicious(config) => {
                Box::new(MaliciousRepairHandler::new(blockstore, *config))
            }
        }
    }

    pub fn create_serve_repair(
        &self,
        blockstore: Arc<Blockstore>,
        cluster_info: Arc<ClusterInfo>,
        sharable_banks: SharableBanks,
        serve_repair_whitelist: Arc<RwLock<HashSet<Pubkey>>>,
    ) -> ServeRepair {
        ServeRepair::new(
            cluster_info,
            sharable_banks,
            serve_repair_whitelist,
            self.to_handler(blockstore),
        )
    }
}
