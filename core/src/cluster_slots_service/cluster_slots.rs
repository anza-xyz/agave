use {
    solana_gossip::{
        cluster_info::ClusterInfo, contact_info::ContactInfo, crds::Cursor, epoch_slots::EpochSlots,
    },
    solana_runtime::bank::Bank,
    solana_sdk::{clock::Slot, pubkey::Pubkey, timing::AtomicInterval},
    std::{
        collections::{btree_map, BTreeMap, HashMap},
        sync::{Arc, Mutex, RwLock},
    },
};

// Limit the size of cluster-slots map in case
// of receiving bogus epoch slots values.
// This also constraints the size of the datastructure
// if we are really far behind.
const CLUSTER_SLOTS_TRIM_SIZE: usize = 1500;

pub(crate) type SlotPubkeys = HashMap</*node:*/ Pubkey, /*stake:*/ u64>;

#[derive(Default)]
pub struct ClusterSlots {
    cluster_slots: RwLock<BTreeMap<Slot, Arc<RwLock<SlotPubkeys>>>>,
    validator_stakes: RwLock<Arc<SlotPubkeys>>,
    epoch: RwLock<Option<u64>>,
    cursor: Mutex<Cursor>,
    last_report: AtomicInterval,
}

impl ClusterSlots {
    pub(crate) fn lookup(&self, slot: Slot) -> Option<Arc<RwLock<SlotPubkeys>>> {
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
                e.push((epoch_slots.from, sender_stake));
            }
        }

        {
            let cluster_slots = self.cluster_slots.read().unwrap();
            for (slot, patches) in slots_to_patch {
                let mut slot_wg = cluster_slots.get(&slot).unwrap().write().unwrap();
                slot_wg.extend(patches);
            }
        }
    }

    /// Returns (number of stored slots, total capacity for frozen slot stakes)
    fn datastructure_size(&self) -> (usize, usize) {
        let cluster_slots = self.cluster_slots.read().unwrap();
        (
            cluster_slots.len(),
            cluster_slots
                .iter()
                .map(|(_slot, slot_pubkeys)| slot_pubkeys.read().unwrap().capacity())
                .sum::<usize>(),
        )
    }

    fn report_cluster_slots_size(&self) {
        if self.last_report.should_update(10_000) {
            let (cluster_slots_stored, total_entries) = self.datastructure_size();
            datapoint_info!(
                "cluster-slots-size",
                ("cluster_slots_stored", cluster_slots_stored, i64),
                ("total_entries", total_entries, i64),
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
    ) -> (Vec<u64>, Vec<usize>) {
        let mut weights = Vec::with_capacity(repair_peers.len());
        let mut indices = Vec::with_capacity(repair_peers.len());

        let Some(slot_peers) = self.lookup(slot) else {
            return (weights, indices);
        };
        let slot_peers = slot_peers.read().unwrap();
        for (index, peer) in repair_peers.iter().enumerate() {
            if let Some(stake) = slot_peers.get(peer.pubkey()) {
                weights.push(stake + 1);
                indices.push(index);
            }
        }
        (weights, indices)
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
        let (stored_slots, capacity) = cs.datastructure_size();
        assert_eq!(stored_slots, CLUSTER_SLOTS_TRIM_SIZE);
        assert_eq!(capacity, 0);
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
        assert_eq!(cs.lookup(1).unwrap().read().unwrap().get(&from2), Some(&20));
        assert_eq!(cs.lookup(4).unwrap().read().unwrap().get(&from1), Some(&10));

        let lg1 = cs.lookup(5).unwrap();
        let lg2 = lg1.read().unwrap();
        assert_eq!(lg2.get(&from1), Some(&10));
        assert_eq!(lg2.get(&from2), Some(&20));
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
        let mut map = HashMap::new();
        let k1 = solana_pubkey::new_rand();
        let k2 = solana_pubkey::new_rand();
        map.insert(k1, u64::MAX / 2);
        map.insert(k2, 0);
        cs.cluster_slots
            .write()
            .unwrap()
            .insert(0, Arc::new(RwLock::new(map)));
        let c1 = ContactInfo::new(k1, /*wallclock:*/ 0, /*shred_version:*/ 0);
        let c2 = ContactInfo::new(k2, /*wallclock:*/ 0, /*shred_version:*/ 0);
        assert_eq!(cs.compute_weights(0, &[c1, c2]), vec![u64::MAX / 4, 1]);
    }

    #[test]
    fn test_best_peer_3() {
        let cs = ClusterSlots::default();
        let mut map = HashMap::new();
        let k1 = solana_pubkey::new_rand();
        let k2 = solana_pubkey::new_rand();
        map.insert(k2, 0);
        cs.cluster_slots
            .write()
            .unwrap()
            .insert(0, Arc::new(RwLock::new(map)));
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
        assert_eq!(
            cs.lookup(1)
                .unwrap()
                .read()
                .unwrap()
                .get(&Pubkey::default()),
            Some(&1)
        );
    }
}
