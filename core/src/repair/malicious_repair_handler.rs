use {
    super::{
        repair_handler::RepairHandler, repair_response::repair_response_packet_from_bytes,
        standard_repair_handler::StandardRepairHandler,
    },
    log::info,
    solana_clock::Slot,
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::Blockstore,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{Nonce, ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
    },
    solana_perf::packet::{Packet, PacketBatch, PacketBatchRecycler},
    solana_signer::Signer,
    std::{net::SocketAddr, sync::Arc},
};

#[derive(Copy, Clone, Debug, Default)]
pub struct MaliciousRepairConfig {
    /// If set, respond maliciously for slots where `slot % frequency == 0`
    pub bad_shred_slot_frequency: Option<Slot>,
    /// If set, respond maliciously for shred indices where `index % frequency == 0`
    pub bad_shred_index_frequency: Option<u64>,
    /// If set, only respond maliciously for slots within this range (inclusive)
    pub slot_range: Option<(Slot, Slot)>,
}

pub struct MaliciousRepairHandler {
    blockstore: Arc<Blockstore>,
    keypair: Arc<Keypair>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    config: MaliciousRepairConfig,
    reed_solomon_cache: ReedSolomonCache,
    standard_repair_handler: StandardRepairHandler,
}

impl MaliciousRepairHandler {
    pub fn new(
        blockstore: Arc<Blockstore>,
        keypair: Arc<Keypair>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        config: MaliciousRepairConfig,
    ) -> Self {
        Self {
            standard_repair_handler: StandardRepairHandler::new(blockstore.clone()),
            blockstore,
            keypair,
            leader_schedule_cache,
            config,
            reed_solomon_cache: ReedSolomonCache::default(),
        }
    }

    /// Check if we should respond maliciously for this slot and shred index
    fn should_respond_maliciously(&self, slot: Slot, shred_index: u64) -> bool {
        if let Some((start, end)) = self.config.slot_range {
            if slot < start || slot > end {
                return false;
            }
        }

        let slot_matches = self
            .config
            .bad_shred_slot_frequency
            .is_some_and(|freq| slot.is_multiple_of(freq));
        let index_matches = self
            .config
            .bad_shred_index_frequency
            .is_some_and(|freq| shred_index.is_multiple_of(freq));

        // If both frequencies are set, both must match
        // If only one is set, that one must match
        match (
            self.config.bad_shred_slot_frequency,
            self.config.bad_shred_index_frequency,
        ) {
            (Some(_), Some(_)) => slot_matches && index_matches,
            (Some(_), None) => slot_matches,
            (None, Some(_)) => index_matches,
            (None, None) => false,
        }
    }

    /// Check if we were the leader for this slot
    fn is_leader_for_slot(&self, slot: Slot) -> bool {
        self.leader_schedule_cache
            .slot_leader_at(slot, None)
            .is_some_and(|leader| leader.id == self.keypair.pubkey())
    }

    /// Generate an equivocating shred - a legitimately signed shred with different data
    fn generate_equivocating_shred(
        &self,
        original_shred: &Shred,
        shred_index: u64,
    ) -> Option<Vec<u8>> {
        let slot = original_shred.slot();
        let parent_slot = original_shred.parent().ok()?;
        let version = original_shred.version();
        // Use 0 for reference_tick since we can't access the private method
        // This is fine for equivocation testing purposes
        let reference_tick = 0u8;

        // Create a shredder with the same slot parameters
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).ok()?;

        // Create fake entries with different data than the original
        // We use a unique hash based on the shred index to ensure different content
        let fake_hash = Hash::new_unique();
        let fake_entries = vec![Entry::new(&fake_hash, 1, vec![])];

        // Generate new shreds signed by our keypair
        let chained_merkle_root = original_shred.chained_merkle_root().ok()?;
        let is_last_in_slot = original_shred.last_in_slot();

        let shreds: Vec<Shred> = shredder
            .make_merkle_shreds_from_entries(
                &self.keypair,
                &fake_entries,
                is_last_in_slot,
                chained_merkle_root,
                shred_index as u32, // next_shred_index
                0,                  // next_code_index
                &self.reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            )
            .collect();

        // Return the first data shred's payload
        shreds
            .into_iter()
            .find(|s| s.is_data())
            .map(|s| s.into_payload().to_vec())
    }
}

impl RepairHandler for MaliciousRepairHandler {
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
        // Get the original shred from blockstore
        let original_shred_bytes = self
            .blockstore
            .get_data_shred(slot, shred_index)
            .expect("Blockstore could not get data shred")?;

        // Only respond maliciously if:
        // 1. We were the leader for this slot (we have the keypair to sign)
        // 2. The slot/index matches our frequency configuration
        if self.is_leader_for_slot(slot) && self.should_respond_maliciously(slot, shred_index) {
            // Parse the original shred to get its metadata
            if let Ok(original_shred) =
                Shred::new_from_serialized_shred(original_shred_bytes.clone())
            {
                if let Some(equivocating_shred) =
                    self.generate_equivocating_shred(&original_shred, shred_index)
                {
                    info!(
                        "Responding with equivocating shred in slot {slot} index {shred_index} to \
                         {dest}"
                    );
                    return repair_response_packet_from_bytes(equivocating_shred, dest, nonce);
                }
            }
        }

        // Fall back to normal response
        repair_response_packet_from_bytes(original_shred_bytes, dest, nonce)
    }

    fn run_orphan(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        max_responses: usize,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        self.standard_repair_handler
            .run_orphan(recycler, from_addr, slot, max_responses, nonce)
    }
}
