use {
    super::{
        malicious_repair_handler::{MaliciousRepairConfig, MaliciousRepairHandler},
        repair_response::repair_response_packet_from_bytes,
        serve_repair::ServeRepair,
        standard_repair_handler::StandardRepairHandler,
    },
    crate::repair::{
        repair_response,
        serve_repair::{AncestorHashesResponse, BlockIdRepairResponse, MAX_ANCESTOR_RESPONSES},
    },
    agave_votor_messages::migration::MigrationStatus,
    bincode::serialize,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_ledger::{
        ancestor_iterator::{AncestorIterator, AncestorIteratorWithHash},
        blockstore::Blockstore,
        shred::{DATA_SHREDS_PER_FEC_BLOCK, ErasureSetId, Nonce},
    },
    solana_perf::packet::{Packet, PacketBatch, PacketBatchRecycler, RecycledPacketBatch},
    solana_poh::poh_recorder::SharedLeaderState,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        collections::HashSet,
        net::SocketAddr,
        sync::{Arc, RwLock},
    },
};

/// Helper function to create a PacketBatch from a serializable response
fn create_response_packet_batch<T: serde::Serialize>(
    recycler: &PacketBatchRecycler,
    response: &T,
    from_addr: &SocketAddr,
    nonce: Nonce,
    debug_label: &'static str,
) -> Option<PacketBatch> {
    let serialized_response = serialize(response).ok()?;
    let packet =
        repair_response::repair_response_packet_from_bytes(serialized_response, from_addr, nonce)?;
    Some(RecycledPacketBatch::new_with_recycler_data(recycler, debug_label, vec![packet]).into())
}

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

    fn run_window_request_for_block_id(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        shred_index: u64,
        block_id: Hash,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        let location = self
            .blockstore()
            .get_block_location(slot, block_id)
            .ok()??;
        let shred = self
            .blockstore()
            .get_data_shred_from_location(slot, shred_index, location)
            .ok()??;
        let packet = repair_response_packet_from_bytes(shred, from_addr, nonce)?;
        Some(
            RecycledPacketBatch::new_with_recycler_data(
                recycler,
                "run_window_request_for_block_id",
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
        create_response_packet_batch(recycler, &response, from_addr, nonce, "run_ancestor_hashes")
    }

    fn run_parent_fec_set_count(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        block_id: Hash,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        let (double_merkle_meta, location) = self
            .blockstore()
            .get_double_merkle_meta_maybe_populate_proofs_for_block_id(slot, block_id)
            .ok()??;

        let slot_meta = self
            .blockstore()
            .meta_from_location(slot, location)
            .ok()??;

        let parent_slot = slot_meta.parent_slot?;
        let parent_block_id = slot_meta.parent_block_id;
        let parent_proof = double_merkle_meta.get_parent_info_proof()?.to_vec();

        let response = BlockIdRepairResponse::ParentFecSetCount {
            fec_set_count: double_merkle_meta.fec_set_count(),
            parent_info: (parent_slot, parent_block_id),
            parent_proof,
        };
        create_response_packet_batch(
            recycler,
            &response,
            from_addr,
            nonce,
            "run_parent_fec_set_count",
        )
    }

    fn run_fec_set_root(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        block_id: Hash,
        fec_set_index: u32,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        let (double_merkle_meta, location) = self
            .blockstore()
            .get_double_merkle_meta_maybe_populate_proofs_for_block_id(slot, block_id)
            .ok()??;

        let fec_set_root = self
            .blockstore()
            .merkle_root_meta_from_location(ErasureSetId::new(slot, fec_set_index), location)
            .ok()??
            .merkle_root()?;
        let proof_index = fec_set_index.checked_div(DATA_SHREDS_PER_FEC_BLOCK as u32)?;
        let fec_set_proof = double_merkle_meta.get_fec_set_proof(proof_index)?.to_vec();

        let response = BlockIdRepairResponse::FecSetRoot {
            fec_set_root,
            fec_set_proof,
        };
        create_response_packet_batch(recycler, &response, from_addr, nonce, "run_fec_set_root")
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
        leader_state: SharedLeaderState,
        migration_status: Arc<MigrationStatus>,
    ) -> ServeRepair {
        ServeRepair::new_with_leader_state(
            cluster_info,
            sharable_banks,
            serve_repair_whitelist,
            self.to_handler(blockstore),
            leader_state,
            migration_status,
        )
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::Rng,
        solana_entry::entry::create_ticks,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore_meta::BlockLocation,
            get_tmp_ledger_path_auto_delete,
            shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
        },
        solana_perf::packet::PacketBatchRecycler,
        std::net::{IpAddr, Ipv4Addr, SocketAddr},
    };

    /// Creates shreds for a slot with both data and coding shreds
    fn setup_erasure_shreds(
        slot: Slot,
        parent_slot: Slot,
        num_entries: u64,
    ) -> (Vec<Shred>, Vec<Shred>) {
        let entries = create_ticks(num_entries, 0, Hash::default());
        let leader_keypair = Arc::new(Keypair::new());
        let shredder = Shredder::new(slot, parent_slot, 0, 0).unwrap();
        let mut rng = rand::rng();
        let chained_merkle_root = Hash::new_from_array(rng.random());
        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            true, // is_last_in_slot
            chained_merkle_root,
            0, // next_shred_index
            0, // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        (data_shreds, coding_shreds)
    }

    /// Sets up a blockstore with a complete slot and all necessary metadata
    fn setup_blockstore_with_complete_slot(
        slot: Slot,
        parent_slot: Slot,
        num_entries: u64,
    ) -> (Arc<Blockstore>, Hash, Vec<Hash>) {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());

        // First, insert the parent slot so the child slot can be marked full
        let grandparent_slot = parent_slot.saturating_sub(1);
        let (parent_data_shreds, parent_coding_shreds) =
            setup_erasure_shreds(parent_slot, grandparent_slot, 10);
        blockstore
            .insert_shreds(
                parent_data_shreds
                    .into_iter()
                    .chain(parent_coding_shreds)
                    .collect::<Vec<_>>(),
                None,
                true,
            )
            .unwrap();

        // Now create the target slot
        let (data_shreds, coding_shreds) = setup_erasure_shreds(slot, parent_slot, num_entries);

        // Collect merkle roots for each FEC set
        let mut fec_set_roots = Vec::new();
        for shred in data_shreds.iter() {
            if shred.index() % (DATA_SHREDS_PER_FEC_BLOCK as u32) == 0 {
                fec_set_roots.push(shred.merkle_root().unwrap());
            }
        }

        // Insert shreds - DoubleMerkleMeta will be computed atomically when slot becomes full
        blockstore
            .insert_shreds(
                data_shreds
                    .into_iter()
                    .chain(coding_shreds)
                    .collect::<Vec<_>>(),
                None,
                true, // is_trusted
            )
            .unwrap();

        // Verify the slot is full
        let slot_meta = blockstore.meta(slot).unwrap().unwrap();
        assert!(
            slot_meta.is_full(),
            "Slot should be full after inserting all shreds"
        );

        // Get the double merkle root (computed during shred insertion when slot became full)
        let block_id = blockstore
            .get_double_merkle_root(slot, BlockLocation::Original)
            .expect("DoubleMerkleMeta fetch should succeed")
            .expect("DoubleMerkleMeta should exist for full slot");

        (blockstore, block_id, fec_set_roots)
    }

    #[test]
    fn test_run_fec_set_root() {
        let slot = 1000;
        let parent_slot = 999;
        // Use many entries to ensure multiple FEC sets (each FEC set has 32 data shreds)
        let num_entries = 2000;

        let (blockstore, block_id, fec_set_roots) =
            setup_blockstore_with_complete_slot(slot, parent_slot, num_entries);

        let handler = StandardRepairHandler::new(blockstore.clone());
        let recycler = PacketBatchRecycler::default();
        let from_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let nonce = 12345;

        // Verify we have multiple FEC sets
        assert!(
            fec_set_roots.len() >= 2,
            "Should have at least 2 FEC sets for this test, got {}",
            fec_set_roots.len()
        );

        // Test each FEC set using the actual fec_set_index (0, 32, 64, ...)
        for (i, expected_root) in fec_set_roots.iter().enumerate() {
            let fec_set_index = (i * DATA_SHREDS_PER_FEC_BLOCK) as u32;

            let result = handler.run_fec_set_root(
                &recycler,
                &from_addr,
                slot,
                block_id,
                fec_set_index,
                nonce,
            );

            assert!(
                result.is_some(),
                "run_fec_set_root should succeed for fec_set_index {fec_set_index}"
            );

            // Deserialize the response and verify
            let packet_batch = result.unwrap();
            assert_eq!(packet_batch.len(), 1);

            let packet = packet_batch.iter().next().unwrap();
            let (response, response_nonce): (BlockIdRepairResponse, Nonce) =
                bincode::deserialize(packet.data(..packet.meta().size).unwrap()).unwrap();

            assert_eq!(response_nonce, nonce);
            match response {
                BlockIdRepairResponse::FecSetRoot {
                    fec_set_root,
                    fec_set_proof,
                } => {
                    assert_eq!(
                        fec_set_root, *expected_root,
                        "FEC set root should match for index {fec_set_index}"
                    );
                    assert!(
                        !fec_set_proof.is_empty(),
                        "FEC set proof should not be empty"
                    );
                }
                _ => panic!("Expected FecSetRoot response"),
            }
        }

        // Test with invalid block_id returns None
        let invalid_block_id = Hash::new_unique();
        let result =
            handler.run_fec_set_root(&recycler, &from_addr, slot, invalid_block_id, 0, nonce);
        assert!(result.is_none(), "Should return None for invalid block_id");

        // Test with out-of-bounds fec_set_index returns None
        let invalid_fec_set_index = (fec_set_roots.len() * DATA_SHREDS_PER_FEC_BLOCK) as u32;
        let result = handler.run_fec_set_root(
            &recycler,
            &from_addr,
            slot,
            block_id,
            invalid_fec_set_index,
            nonce,
        );
        assert!(
            result.is_none(),
            "Should return None for out-of-bounds fec_set_index"
        );
    }

    #[test]
    fn test_run_parent_fec_set_count() {
        let slot = 1000;
        let parent_slot = 999;
        let num_entries = 2000;

        let (blockstore, block_id, fec_set_roots) =
            setup_blockstore_with_complete_slot(slot, parent_slot, num_entries);

        let handler = StandardRepairHandler::new(blockstore.clone());
        let recycler = PacketBatchRecycler::default();
        let from_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let nonce = 12345;

        let result = handler.run_parent_fec_set_count(&recycler, &from_addr, slot, block_id, nonce);

        assert!(result.is_some(), "run_parent_fec_set_count should succeed");

        // Deserialize and verify the response
        let packet_batch = result.unwrap();
        assert_eq!(packet_batch.len(), 1);

        let packet = packet_batch.iter().next().unwrap();
        let (response, response_nonce): (BlockIdRepairResponse, Nonce) =
            bincode::deserialize(packet.data(..packet.meta().size).unwrap()).unwrap();

        assert_eq!(response_nonce, nonce);
        match response {
            BlockIdRepairResponse::ParentFecSetCount {
                fec_set_count,
                parent_info: (p_slot, p_block_id),
                parent_proof,
            } => {
                assert_eq!(
                    fec_set_count as usize,
                    fec_set_roots.len(),
                    "FEC set count should match"
                );
                assert_eq!(p_slot, parent_slot, "Parent slot should match");
                assert_eq!(
                    p_block_id,
                    Hash::default(),
                    "Parent block ID should be default"
                );

                assert!(!parent_proof.is_empty(), "Parent proof should not be empty");
            }
            _ => panic!("Expected ParentFecSetCount response"),
        }

        // Test with invalid block_id returns None
        let invalid_block_id = Hash::new_unique();
        let result =
            handler.run_parent_fec_set_count(&recycler, &from_addr, slot, invalid_block_id, nonce);
        assert!(result.is_none(), "Should return None for invalid block_id");

        // Test with non-existent slot returns None
        let result = handler.run_parent_fec_set_count(&recycler, &from_addr, 9999, block_id, nonce);
        assert!(result.is_none(), "Should return None for non-existent slot");
    }
}
