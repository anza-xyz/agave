use {
    dashmap::DashMap,
    solana_gossip::{
        cluster_info::ClusterInfo, contact_info::ContactInfo, crds::Cursor, epoch_slots::EpochSlots,
    },
    solana_runtime::bank::Bank,
    solana_sdk::{clock::Slot, pubkey::Pubkey, timing::AtomicInterval},
    std::ops::Range,
    std::{
        collections::{HashMap, VecDeque},
        sync::{Arc, Mutex, RwLock},
    },
};

// Limit the size of cluster-slots map in case
// of receiving bogus epoch slots values.
// This also constraints the size of the datastructure
// if we are really really far behind.
const CLUSTER_SLOTS_TRIM_SIZE: usize = 50000;
// Make hashmaps this many times bigger to reduce collisions
const HASHMAP_OVERSIZE: usize = 2;

pub(crate) type SlotPubkeys = DashMap</*node:*/ Pubkey, /*stake:*/ u64>;

#[derive(Default)]
pub struct ClusterSlots {
    cluster_slots: RwLock<VecDeque<(Slot, Arc<SlotPubkeys>)>>,
    validator_stakes: RwLock<Arc<HashMap<Pubkey, u64>>>,
    epoch: RwLock<Option<u64>>,
    cursor: Mutex<Cursor>,
    last_report: AtomicInterval,
}

impl ClusterSlots {
    pub(crate) fn lookup(&self, slot: Slot) -> Option<Arc<SlotPubkeys>> {
        let cluster_slots = self.cluster_slots.read().unwrap();
        Self::_lookup(slot, &cluster_slots).cloned()
    }

    fn _lookup(
        slot: Slot,
        cluster_slots: &VecDeque<(Slot, Arc<SlotPubkeys>)>,
    ) -> Option<&Arc<SlotPubkeys>> {
        let idx = cluster_slots
            .binary_search_by_key(&slot, |(s, _)| *s)
            .ok()?;
        Some(&cluster_slots[idx].1)
    }

    pub(crate) fn update(
        &self,
        root_bank: &Bank,
        validator_stakes: &Arc<HashMap<Pubkey, u64>>,
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

    // Initialize cluster_slots during startup
    // This should not be called during normal operation!
    fn initialize_cluster_slots(
        slot_range: &Range<Slot>,
        cluster_slots: &mut VecDeque<(Slot, Arc<SlotPubkeys>)>,
        capacity: usize,
    ) {
        assert!(cluster_slots.is_empty());
        for slot in slot_range.clone() {
            cluster_slots.push_back((slot, Arc::new(DashMap::with_capacity(capacity))));
        }
    }

    fn update_internal(
        &self,
        root: Slot,
        validator_stakes: &HashMap<Pubkey, u64>,
        epoch_slots_list: Vec<EpochSlots>,
        num_epoch_slots: u64,
    ) {
        // Discard slots at or before current root or too far ahead.
        let slot_range = (root + 1)
            ..root.saturating_add(num_epoch_slots.min(CLUSTER_SLOTS_TRIM_SIZE as u64 + 1));
        // ensure the datastructure has the correct window in scope
        {
            let map_capacity = validator_stakes.len() * HASHMAP_OVERSIZE;
            let mut cluster_slots = self.cluster_slots.write().unwrap();

            if cluster_slots.is_empty() {
                Self::initialize_cluster_slots(&slot_range, &mut cluster_slots, map_capacity);
            }
            // discard and recycle outdated elements
            loop {
                let (slot, _) = cluster_slots
                    .front()
                    .expect("After initialization the ring buffer is not empty");
                if *slot < root + 1 {
                    // pop useless record from the front
                    let (_, map) = cluster_slots.pop_front().unwrap();
                    // try to reuse its map allocation at the back of the datastructure
                    let slot = cluster_slots.back().unwrap().0 + 1;
                    let map = match Arc::try_unwrap(map) {
                        Ok(map) => {
                            map.clear();
                            map
                        }
                        // if we can not reuse just allocate a new one
                        Err(_) => DashMap::with_capacity(map_capacity),
                    };
                    cluster_slots.push_back((slot, Arc::new(map)));
                } else {
                    break;
                }
            }
            debug_assert!(cluster_slots.len() == CLUSTER_SLOTS_TRIM_SIZE);
        }

        let mut slots_to_patch = HashMap::with_capacity(1024);
        for epoch_slots in epoch_slots_list {
            
            //filter out unstaked nodes
            let Some(sender_stake) = validator_stakes.get(&epoch_slots.from) else {
                continue;
            };
            /*
            let sender_stake = validator_stakes
                .get(&epoch_slots.from)
                .cloned()
                .unwrap_or(0);*/
            let sender_stake = *sender_stake;
            let updates = epoch_slots
                .to_slots(root)
                .filter(|slot| slot_range.contains(slot));
            // figure out which entries would get updated by the new message and cache them
            for slot in updates {
                let e = slots_to_patch
                    .entry(slot)
                    .or_insert_with(|| Vec::with_capacity(128));
                e.push((epoch_slots.from, sender_stake));
            }
        }

        {
            let cluster_slots = self.cluster_slots.read().unwrap();
            for (slot, patches) in slots_to_patch {
                let map = Self::_lookup(slot, &cluster_slots).unwrap();

                for (key, stake) in patches {
                    map.insert(key, stake);
                }
            }
        }
    }

    /// Returns number of stored entries
    fn datastructure_size(&self) -> usize {
        let cluster_slots = self.cluster_slots.read().unwrap();

        cluster_slots
            .iter()
            .map(|(_slot, slot_pubkeys)| slot_pubkeys.len())
            .sum::<usize>()
    }

    fn report_cluster_slots_size(&self) {
        if self.last_report.should_update(10_000) {
            datapoint_info!(
                "cluster-slots-size",
                ("total_entries", self.datastructure_size() as i64, i64),
            );
        }
    }

    #[cfg(test)]
    // patches the given node_id into the internal structures
    // to pretend as if it has submitted epoch slots for a given slot
    pub(crate) fn insert_node_id(&self, slot: Slot, node_id: Pubkey) {
        let balance = self
            .validator_stakes
            .read()
            .unwrap()
            .get(&node_id)
            .cloned()
            .unwrap_or(0);
        if let Some(slot_pubkeys) = self.lookup(slot) {
            slot_pubkeys.insert(node_id, balance);
        } else {
            let mut cluster_slots = self.cluster_slots.write().unwrap();
            cluster_slots.push_back((slot, Arc::new(DashMap::from_iter([(node_id, balance)]))));
            cluster_slots.make_contiguous().sort_by_key(|(k, _)| *k);
        }
    }

    fn update_peers(&self, staked_nodes: &Arc<HashMap<Pubkey, u64>>, root_bank: &Bank) {
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
        repair_peers
            .iter()
            .map(|peer| slot_peers.get(peer.pubkey()).map(|v| *v).unwrap_or(0))
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
                repair_peers
                    .iter()
                    .enumerate()
                    .filter_map(|(i, ci)| Some((*slot_peers.get(ci.pubkey())? + 1, i)))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_sdk::clock::DEFAULT_SLOTS_PER_EPOCH};

    #[test]
    fn test_default() {
        let cs = ClusterSlots::default();
        assert!(cs.cluster_slots.read().unwrap().is_empty());
    }

    #[test]
    fn test_update_noop() {
        let cs = ClusterSlots::default();
        cs.update_internal(0, &HashMap::new(), vec![], DEFAULT_SLOTS_PER_EPOCH);
        let stored_slots = cs.datastructure_size();
        assert_eq!(stored_slots, 0);
    }

    #[test]
    fn test_update_empty() {
        let cs = ClusterSlots::default();
        let epoch_slot = EpochSlots::default();
        cs.update_internal(
            0,
            &HashMap::new(),
            vec![epoch_slot],
            DEFAULT_SLOTS_PER_EPOCH,
        );
        assert!(cs.lookup(0).is_none());
    }

    #[test]
    fn test_update_rooted() {
        //root is 0, so it should clear out the slot
        let cs = ClusterSlots::default();
        let mut epoch_slot = EpochSlots::default();
        epoch_slot.fill(&[0], 0);
        cs.update_internal(
            0,
            &HashMap::new(),
            vec![epoch_slot],
            DEFAULT_SLOTS_PER_EPOCH,
        );
        assert!(cs.lookup(0).is_none());
    }

    #[test]
    fn test_update_new_multiple_slots() {
        let cs = ClusterSlots::default();
        let mut epoch_slot1 = EpochSlots { from: Pubkey::new_unique(), ..Default::default() };
        epoch_slot1.fill(&[2, 4, 5], 0);
        let from1 = epoch_slot1.from;
        let mut epoch_slot2 = EpochSlots { from: Pubkey::new_unique(), ..Default::default() };
        epoch_slot2.fill(&[1, 3, 5], 1);
        let from2 = epoch_slot2.from;
        cs.update_internal(
            0,
            &HashMap::from([(from1, 10), (from2, 20)]),
            vec![epoch_slot1, epoch_slot2],
            DEFAULT_SLOTS_PER_EPOCH,
        );
        assert!(cs.lookup(0).is_none());
        assert!(cs.lookup(1).is_some());
        assert_eq!(*cs.lookup(1).unwrap().get(&from2).unwrap(), 20);
        assert_eq!(*cs.lookup(4).unwrap().get(&from1).unwrap(), 10);

        let map = cs.lookup(5).unwrap();
        assert_eq!(*map.get(&from1).unwrap(), 10);
        assert_eq!(*map.get(&from2).unwrap(), 20);
    }

    #[test]
    fn test_compute_weights() {
        let cs = ClusterSlots::default();
        let ci = ContactInfo::default();
        assert_eq!(cs.compute_weights(0, &[ci]), vec![1]);
    }

    #[test]
    fn test_best_peer_2() {
        let cs = ClusterSlots::default();
        let map = DashMap::new();
        let k1 = solana_pubkey::new_rand();
        let k2 = solana_pubkey::new_rand();
        map.insert(k1, u64::MAX / 2);
        map.insert(k2, 0);
        cs.cluster_slots
            .write()
            .unwrap()
            .push_back((0, Arc::new(map)));
        let c1 = ContactInfo::new(k1, /*wallclock:*/ 0, /*shred_version:*/ 0);
        let c2 = ContactInfo::new(k2, /*wallclock:*/ 0, /*shred_version:*/ 0);
        assert_eq!(cs.compute_weights(0, &[c1, c2]), vec![u64::MAX / 4, 1]);
    }

    #[test]
    fn test_best_peer_3() {
        let cs = ClusterSlots::default();
        let map = DashMap::new();
        let k1 = solana_pubkey::new_rand();
        let k2 = solana_pubkey::new_rand();
        map.insert(k2, 0);
        cs.cluster_slots
            .write()
            .unwrap()
            .push_back((0, Arc::new(map)));
        //make sure default weights are used as well
        let validator_stakes: HashMap<_, _> = vec![(k1, u64::MAX / 2)].into_iter().collect();
        *cs.validator_stakes.write().unwrap() = Arc::new(validator_stakes);
        let c1 = ContactInfo::new(k1, /*wallclock:*/ 0, /*shred_version:*/ 0);
        let c2 = ContactInfo::new(k2, /*wallclock:*/ 0, /*shred_version:*/ 0);
        assert_eq!(cs.compute_weights(0, &[c1, c2]), vec![u64::MAX / 4 + 1, 1]);
    }

    #[test]
    fn test_best_completed_slot_peer() {
        let cs = ClusterSlots::default();
        let contact_infos: Vec<_> = std::iter::repeat_with(|| {
            ContactInfo::new(
                solana_pubkey::new_rand(),
                0, // wallclock
                0, // shred_version
            )
        })
        .take(2)
        .collect();
        let slot = 9;

        // None of these validators have completed slot 9, so should
        // return nothing
        let (w, i) = cs.compute_weights_exclude_nonfrozen(slot, &contact_infos);
        assert!(w.is_empty());
        assert!(i.is_empty());

        // Give second validator max stake
        let validator_stakes: HashMap<_, _> = vec![(*contact_infos[1].pubkey(), u64::MAX / 2)]
            .into_iter()
            .collect();
        *cs.validator_stakes.write().unwrap() = Arc::new(validator_stakes);

        // Mark the first validator as completed slot 9, should pick that validator,
        // even though it only has default stake, while the other validator has
        // max stake
        cs.insert_node_id(slot, *contact_infos[0].pubkey());
        let (w, i) = cs.compute_weights_exclude_nonfrozen(slot, &contact_infos);
        assert_eq!(w, [1]);
        assert_eq!(i, [0]);
    }

    #[test]
    fn test_update_new_staked_slot() {
        let cs = ClusterSlots::default();
        let mut epoch_slot = EpochSlots::default();
        epoch_slot.fill(&[1], 0);

        let map = vec![(Pubkey::default(), 1)].into_iter().collect();

        cs.update_internal(0, &map, vec![epoch_slot], DEFAULT_SLOTS_PER_EPOCH);
        assert!(cs.lookup(1).is_some());
        assert_eq!(*cs.lookup(1).unwrap().get(&Pubkey::default()).unwrap(), 1);
    }
}
