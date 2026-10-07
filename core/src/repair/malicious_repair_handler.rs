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
        shred::{
            DATA_SHREDS_PER_FEC_BLOCK, Nonce, ProcessShredsStats, ReedSolomonCache, Shred, Shredder,
        },
    },
    solana_perf::packet::{BytesPacket, PacketBatch},
    solana_signer::Signer,
    std::{
        collections::BTreeMap,
        net::{IpAddr, SocketAddr},
        sync::{Arc, RwLock},
    },
};

/// A dynamically configurable set of malicious repair responses, indexed by the
/// requested shred slot and the repair requester's IP address.
///
/// The handle is cloneable so a local-cluster test can retain a controller while
/// the validator's repair service owns another clone.
#[derive(Clone, Debug, Default)]
pub struct MaliciousRepairSchedule {
    faults: Arc<RwLock<BTreeMap<Slot, BTreeMap<IpAddr, u64>>>>,
}

impl MaliciousRepairSchedule {
    /// Replaces the scheduled recipient-to-mutation-seed mapping for `slot`.
    pub fn set_slot_faults(&self, slot: Slot, faults: BTreeMap<IpAddr, u64>) {
        let mut schedule = self.faults.write().unwrap();
        if faults.is_empty() {
            schedule.remove(&slot);
        } else {
            schedule.insert(slot, faults);
        }
    }

    /// Removes all scheduled malicious responses for `slot`.
    pub fn clear_slot(&self, slot: Slot) {
        self.faults.write().unwrap().remove(&slot);
    }

    fn mutation_seed(&self, slot: Slot, shred_index: u64, recipient: IpAddr) -> Option<u64> {
        self.faults
            .read()
            .unwrap()
            .get(&slot)
            .and_then(|faults| faults.get(&recipient))
            .copied()
            // Mutating every independently repaired shred would create internally
            // inconsistent FEC sets. Use the seed to choose one shred per FEC set,
            // matching the granularity of the existing equivocation tests.
            .filter(|seed| {
                shred_index % DATA_SHREDS_PER_FEC_BLOCK as u64
                    == seed % DATA_SHREDS_PER_FEC_BLOCK as u64
            })
    }
}

#[derive(Clone, Debug, Default)]
pub struct MaliciousRepairConfig {
    /// If set, respond maliciously for slots where `slot % frequency == 0`
    pub bad_shred_slot_frequency: Option<Slot>,
    /// If set, respond maliciously for shred indices where `index % frequency == 0`
    pub bad_shred_index_frequency: Option<u64>,
    /// If set, only respond maliciously for slots within this range (inclusive)
    pub slot_range: Option<(Slot, Slot)>,
    /// Optional dynamic process-fault schedule used by local-cluster tests.
    pub fault_schedule: Option<MaliciousRepairSchedule>,
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
        if let Some((start, end)) = self.config.slot_range
            && (slot < start || slot > end)
        {
            return false;
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
        mutation_seed: Option<u64>,
    ) -> Option<Vec<u8>> {
        let slot = original_shred.slot();
        let parent_slot = original_shred.parent().ok()?;
        let version = original_shred.version();
        // Use 0 for reference_tick since we can't access the private method
        // This is fine for equivocation testing purposes
        let reference_tick = 0u8;

        // Create a shredder with the same slot parameters
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).ok()?;

        // Scheduled faults use a reproducible mutation. The legacy frequency controls
        // retain their previous unique-per-response behavior.
        let fake_hash = mutation_seed.map_or_else(Hash::new_unique, |seed| {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&seed.to_le_bytes());
            bytes[8..16].copy_from_slice(&slot.to_le_bytes());
            bytes[16..24].copy_from_slice(&shred_index.to_le_bytes());
            bytes[24..].copy_from_slice(b"byzfuzz!");
            Hash::new_from_array(bytes)
        });
        let fake_entries = vec![Entry::new(&fake_hash, 1, vec![])];

        // Generate new shreds signed by our keypair
        let chained_merkle_root = original_shred.chained_merkle_root().ok()?;
        let is_last_in_slot = original_shred.last_in_slot();

        let shreds = shredder.make_merkle_shreds_from_entries(
            &self.keypair,
            &fake_entries,
            is_last_in_slot,
            chained_merkle_root,
            shred_index as u32, // next_shred_index
            0,                  // next_code_index
            &self.reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );

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
    ) -> Option<BytesPacket> {
        // Get the original shred from blockstore
        let original_shred_bytes = self
            .blockstore
            .get_data_shred(slot, shred_index)
            .expect("Blockstore could not get data shred")?;

        // Only respond maliciously if:
        // 1. We were the leader for this slot (we have the keypair to sign)
        // 2. The slot/index matches our frequency configuration
        let mutation_seed = self
            .config
            .fault_schedule
            .as_ref()
            .and_then(|schedule| schedule.mutation_seed(slot, shred_index, dest.ip()));
        let should_respond_maliciously =
            mutation_seed.is_some() || self.should_respond_maliciously(slot, shred_index);
        if self.is_leader_for_slot(slot) && should_respond_maliciously {
            // Parse the original shred to get its metadata
            if let Ok(original_shred) =
                Shred::new_from_serialized_shred(original_shred_bytes.clone())
                && let Some(equivocating_shred) =
                    self.generate_equivocating_shred(&original_shred, shred_index, mutation_seed)
            {
                info!(
                    "Responding with equivocating shred in slot {slot} index {shred_index} to \
                     {dest}; mutation_seed={mutation_seed:?}"
                );
                return repair_response_packet_from_bytes(equivocating_shred, dest, nonce);
            }
        }

        // Fall back to normal response
        repair_response_packet_from_bytes(original_shred_bytes, dest, nonce)
    }

    fn run_orphan(
        &self,
        from_addr: &SocketAddr,
        slot: Slot,
        max_responses: usize,
        nonce: Nonce,
    ) -> Option<PacketBatch> {
        self.standard_repair_handler
            .run_orphan(from_addr, slot, max_responses, nonce)
    }
}
