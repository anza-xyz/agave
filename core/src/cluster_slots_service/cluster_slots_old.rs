use {
    itertools::Itertools,
    solana_gossip::{
        cluster_info::ClusterInfo, contact_info::ContactInfo, crds::Cursor, epoch_slots::EpochSlots,
    },
    solana_runtime::bank::Bank,
    solana_sdk::{clock::Slot, pubkey::Pubkey, timing::AtomicInterval},
    std::{
        collections::{btree_map, BTreeMap, HashMap},
        ops::Range,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Mutex, RwLock,
        },
    },
};

// Limit the size of cluster-slots map in case
// of receiving bogus epoch slots values.
// This also constraints the size of the datastructure
// if we are really far behind.
const CLUSTER_SLOTS_TRIM_SIZE: usize = 50000;

pub(crate) type SlotPubkeys = HashMap</*node:*/ Pubkey, /*stake:*/ u64>;
#[allow(dead_code)]
#[derive(Default)]
pub struct ClusterSlots {
    cluster_slots: RwLock<BTreeMap<Slot, Arc<RwLock<SlotPubkeys>>>>,
    validator_stakes: RwLock<Arc<SlotPubkeys>>,
    pub total_writes: AtomicU64,
    epoch: RwLock<Option<u64>>,
    cursor: Mutex<Cursor>,
    last_report: AtomicInterval,
}
#[allow(dead_code)]
impl ClusterSlots {
    pub fn lookup(&self, slot: Slot) -> Option<Arc<RwLock<SlotPubkeys>>> {
        self.cluster_slots.read().unwrap().get(&slot).cloned()
    }

    pub(crate) fn update(
        &self,
        root_bank: &Bank,
        validator_stakes: &Arc<SlotPubkeys>,
        cluster_info: &ClusterInfo,
    ) {
        self.update_peers(validator_stakes, root_bank);
        let epoch_slots = {
            let mut cursor = self.cursor.lock().unwrap();
            cluster_info.get_epoch_slots(&mut cursor)
        };
        let num_epoch_slots = root_bank.get_slots_in_epoch(root_bank.epoch());
        self.update_internal(
            root_bank.slot(),
            validator_stakes,
            epoch_slots,
            num_epoch_slots,
        );
        self.report_cluster_slots_size();
    }

    fn update_internal(
        &self,
        root: Slot,
        validator_stakes: &SlotPubkeys,
        epoch_slots_list: Vec<EpochSlots>,
        num_epoch_slots: u64,
    ) {
        // Discard slots at or before current root or too far ahead.
        let slot_range = (root + 1)
            ..root.saturating_add(num_epoch_slots.min(CLUSTER_SLOTS_TRIM_SIZE as u64 + 1));
        {
            let mut cluster_slots = self.cluster_slots.write().unwrap();
            if cluster_slots.contains_key(&root) {
                // split off possibly outdated old entries
                *cluster_slots = cluster_slots.split_off(&(root + 1));
            }

            // ensure entries in cluster_slots are present for any valid epochslot we might get
            // to avoid excessive write-locking
            for slot in slot_range.clone().rev() {
                match cluster_slots.entry(slot) {
                    btree_map::Entry::Vacant(entry) => {
                        entry.insert(Arc::new(RwLock::new(HashMap::new())));
                    }
                    //once we hit existing entry, the previous slots should be already populated
                    btree_map::Entry::Occupied(_) => break,
                }
            }
        }

        let mut slots_to_patch = HashMap::with_capacity(1024);
        for epoch_slots in epoch_slots_list {
            //filter out unstaked nodes
            let Some(&sender_stake) = validator_stakes.get(&epoch_slots.from) else {
                continue;
            };
            /*
            let sender_stake = validator_stakes
                .get(&epoch_slots.from)
                .cloned()
                .unwrap_or(0);*/
            let updates = epoch_slots
                .to_slots(root)
                .filter(|slot| slot_range.contains(slot));
            // figure out which entries would get updated by the new message and cache them
            for slot in updates {
                let e = slots_to_patch
                    .entry(slot)
                    .or_insert_with(|| Vec::with_capacity(128));
                self.total_writes.fetch_add(1, Ordering::Relaxed);
                e.push((epoch_slots.from, sender_stake));
            }
        }

        {
            let cluster_slots = self.cluster_slots.read().unwrap();
            Self::write_updates(&cluster_slots, slots_to_patch);
        }
    }

    fn write_updates(
        cluster_slots: &BTreeMap<Slot, Arc<RwLock<SlotPubkeys>>>,
        slots_to_patch: HashMap<Slot, Vec<(Pubkey, u64)>>,
    ) {
        for (slot, patches) in slots_to_patch {
            let map = cluster_slots.get(&slot).unwrap();
            let mut map_wg = map.write().unwrap();
            map_wg.extend(patches);
        }
    }
    #[cfg(feature = "dev-context-only-utils")]
    pub fn generate_fill_for_tests(
        &self,
        stakes: &HashMap<Pubkey, u64>,
        root: Slot,
        slots: Range<Slot>,
    ) {
        assert!(slots.start > root);
        let epochslots =
            crate::cluster_slots_service::cluster_slots::make_epoch_slots(stakes, slots);
        self.update_internal(root, stakes, epochslots, 100000000);
    }
    #[cfg(feature = "dev-context-only-utils")]
    pub fn generate_fill_for_tests_fp(
        &self,
        stakes: &HashMap<Pubkey, u64>,
        root: u64,
        slots: Range<Slot>,
    ) {
        let epochslots =
            crate::cluster_slots_service::cluster_slots::make_epoch_slots(stakes, slots);
        self.update_internal_fp(root, stakes, epochslots, 100000000);
    }
    fn update_internal_fp(
        &self,
        root: Slot,
        validator_stakes: &SlotPubkeys,
        epoch_slots_list: Vec<EpochSlots>,
        num_epoch_slots: u64,
    ) {
        // Attach validator's total stake.
        let epoch_slots_list: Vec<_> = {
            epoch_slots_list
                .into_iter()
                .filter_map(|epoch_slots| {
                    validator_stakes
                        .get(&epoch_slots.from)
                        .map(|&stake| (epoch_slots, stake))
                })
                .collect()
        };
        // Discard slots at or before current root or too far ahead.
        let slot_range =
            (root + 1)..root.saturating_add(num_epoch_slots.min(CLUSTER_SLOTS_TRIM_SIZE as u64));
        let slot_nodes_stakes = epoch_slots_list
            .iter()
            .flat_map(|(epoch_slots, stake)| {
                epoch_slots
                    .to_slots(root)
                    .filter(|slot| slot_range.contains(slot))
                    .zip(std::iter::repeat((epoch_slots.from, *stake)))
            })
            .into_group_map();
        let slot_nodes_stakes: Vec<_> = {
            let mut cluster_slots = self.cluster_slots.write().unwrap();
            slot_nodes_stakes
                .into_iter()
                .map(|(slot, nodes_stakes)| {
                    let slot_nodes = cluster_slots.entry(slot).or_default().clone();
                    (slot_nodes, nodes_stakes)
                })
                .collect()
        };
        for (slot_nodes, nodes_stakes) in slot_nodes_stakes {
            slot_nodes.write().unwrap().extend(nodes_stakes);
        }
        {
            let mut cluster_slots = self.cluster_slots.write().unwrap();
            *cluster_slots = cluster_slots.split_off(&(root + 1));
            // Allow 10% overshoot so that the computation cost is amortized
            // down. The slots furthest away from the root are discarded.
            if 10 * cluster_slots.len() > 11 * CLUSTER_SLOTS_TRIM_SIZE {
                warn!("trimming cluster slots");
                let key = *cluster_slots.keys().nth(CLUSTER_SLOTS_TRIM_SIZE).unwrap();
                cluster_slots.split_off(&key);
            }
        }
    }

    fn report_cluster_slots_size(&self) {
        if self.last_report.should_update(10_000) {
            let (cluster_slots_cap, pubkeys_capacity) = {
                let cluster_slots = self.cluster_slots.read().unwrap();
                let cluster_slots_cap = cluster_slots.len();
                let pubkeys_capacity = cluster_slots
                    .iter()
                    .map(|(_slot, slot_pubkeys)| slot_pubkeys.read().unwrap().capacity())
                    .sum::<usize>();
                (cluster_slots_cap, pubkeys_capacity)
            };
            datapoint_info!(
                "cluster-slots-size",
                ("cluster_slots_capacity", cluster_slots_cap, i64),
                ("pubkeys_capacity", pubkeys_capacity, i64),
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_node_id(&self, slot: Slot, node_id: Pubkey) {
        let balance = self
            .validator_stakes
            .read()
            .unwrap()
            .get(&node_id)
            .cloned()
            .unwrap_or(0);
        let slot_pubkeys = self
            .cluster_slots
            .write()
            .unwrap()
            .entry(slot)
            .or_default()
            .clone();
        slot_pubkeys.write().unwrap().insert(node_id, balance);
    }

    fn update_peers(&self, staked_nodes: &Arc<SlotPubkeys>, root_bank: &Bank) {
        let root_epoch = root_bank.epoch();
        let my_epoch = *self.epoch.read().unwrap();

        if Some(root_epoch) != my_epoch {
            *self.validator_stakes.write().unwrap() = staked_nodes.clone();
            *self.epoch.write().unwrap() = Some(root_epoch);
        }
    }

    pub(crate) fn compute_weights(&self, slot: Slot, repair_peers: &[ContactInfo]) -> Vec<u64> {
        if repair_peers.is_empty() {
            return Vec::default();
        }
        let stakes = {
            let validator_stakes = self.validator_stakes.read().unwrap();
            repair_peers
                .iter()
                .map(|peer| validator_stakes.get(peer.pubkey()).cloned().unwrap_or(0) + 1)
                .collect()
        };
        let Some(slot_peers) = self.lookup(slot) else {
            return stakes;
        };
        let slot_peers = slot_peers.read().unwrap();
        repair_peers
            .iter()
            .map(|peer| slot_peers.get(peer.pubkey()).cloned().unwrap_or(0))
            .zip(stakes)
            .map(|(a, b)| (a / 2 + b / 2).max(1u64))
            .collect()
    }

    pub(crate) fn compute_weights_exclude_nonfrozen(
        &self,
        slot: Slot,
        repair_peers: &[ContactInfo],
    ) -> Vec<(u64, usize)> {
        self.lookup(slot)
            .map(|slot_peers| {
                let slot_peers = slot_peers.read().unwrap();
                repair_peers
                    .iter()
                    .enumerate()
                    .filter_map(|(i, ci)| Some((slot_peers.get(ci.pubkey())? + 1, i)))
                    .collect()
            })
            .unwrap_or_default()
    }
}
