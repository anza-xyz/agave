use {
    crate::replay_stage::DUPLICATE_THRESHOLD,
    solana_gossip::{
        cluster_info::ClusterInfo, contact_info::ContactInfo, crds::Cursor, epoch_slots::EpochSlots,
    },
    solana_sdk::{clock::Slot, pubkey::Pubkey, timing::AtomicInterval},
    std::{
        collections::{HashMap, VecDeque},
        hash::RandomState,
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
// if we are really really far behind.
const CLUSTER_SLOTS_TRIM_SIZE: usize = 50000;
// Make hashmaps this many times bigger to reduce collisions
const HASHMAP_OVERSIZE: usize = 2;

pub type Stake = u64;

//TODO: switch to solana_pubkey::PubkeyHasherBuilder
type PubkeyHasherBuilder = RandomState;
pub(crate) type ValidatorStakesMap = HashMap<Pubkey, Stake, PubkeyHasherBuilder>;

pub fn make_epoch_slots(stakes: &HashMap<Pubkey, u64>, slots: Range<u64>) -> Vec<EpochSlots> {
    let mut epochslots = Vec::with_capacity(stakes.len());
    let slots_vec: Vec<u64> = slots.clone().collect();
    if !slots_vec.is_empty() {
        for (pk, _) in stakes.iter() {
            let mut epoch_slot = EpochSlots {
                from: *pk,
                ..Default::default()
            };

            epoch_slot.fill(&slots_vec, slots.start);
            epochslots.push(epoch_slot);
        }
    }
    epochslots
}

pub(crate) type SlotSupporters =
    HashMap<Pubkey, AtomicU64 /* stake supporting */, PubkeyHasherBuilder>;
type RowContent = (
    Slot,
    AtomicU64, /*total support */
    Arc<RwLock<SlotSupporters>>,
);

/// amount of stake that needs to lock the slot for us to stop updating it
const FREEZE_THRESHOLD: f64 = 0.8;
// whatever freeze threshold we should be above DUPLICATE_THRESHOLD in order to not break consensus
static_assertions::const_assert!(FREEZE_THRESHOLD > DUPLICATE_THRESHOLD * 1.1);

#[derive(Default)]
pub struct ClusterSlots {
    cluster_slots: RwLock<VecDeque<RowContent>>,
    validator_stakes: RwLock<Arc<ValidatorStakesMap>>,
    total_stake: AtomicU64,
    pub total_writes: AtomicU64,
    pub total_allocations: AtomicU64,
    current_slot: AtomicU64,
    epoch: RwLock<Option<u64>>,
    cursor: Mutex<Cursor>,
    last_report: AtomicInterval,
}

impl ClusterSlots {
    #[inline]
    pub fn lookup(&self, slot: Slot) -> Option<Arc<RwLock<SlotSupporters>>> {
        let cluster_slots = self.cluster_slots.read().unwrap();
        Some(Self::_lookup(slot, &cluster_slots)?.2.clone())
    }

    #[inline]
    fn _lookup(slot: Slot, cluster_slots: &VecDeque<RowContent>) -> Option<&RowContent> {
        let start = cluster_slots.front()?.0;
        if slot < start {
            return None;
        }
        let idx = slot - start;
        cluster_slots.get(idx as usize)
    }

    pub(crate) fn update(
        &self,
        root_slot: Slot,
        validator_stakes: &Arc<ValidatorStakesMap>,
        cluster_info: &ClusterInfo,
        root_epoch: u64,
    ) {
        self.maybe_update_validator_stakes(validator_stakes, root_epoch);
        let epoch_slots = {
            let mut cursor = self.cursor.lock().unwrap();
            cluster_info.get_epoch_slots(&mut cursor)
        };
        self.update_internal(root_slot, validator_stakes, epoch_slots);
        self.report_cluster_slots_size();
    }

    // Advance the cluster_slots ringbuffer, initialize if needed
    fn roll_cluster_slots(
        &self,
        validator_stakes: &HashMap<Pubkey, u64>,
        slot_range: &Range<Slot>,
    ) {
        let current_slot = self.current_slot.swap(slot_range.start, Ordering::Relaxed);
        if current_slot >= slot_range.start {
            return;
        }
        let map_capacity = validator_stakes.len() * HASHMAP_OVERSIZE;
        let mut cluster_slots = self.cluster_slots.write().unwrap();
        self.total_writes.fetch_add(1, Ordering::Relaxed);
        // a handy closure to spawn new hashmaps with the correct parameters
        let map_maker = || {
            let mut map =
                HashMap::with_capacity_and_hasher(map_capacity, PubkeyHasherBuilder::default());
            map.extend(
                validator_stakes
                    .iter()
                    .map(|(k, _v)| (*k, AtomicU64::new(0))),
            );
            self.total_allocations.fetch_add(1, Ordering::Relaxed);
            map
        };
        //startup init, this is very slow but only ever happens once
        if cluster_slots.is_empty() {
            for slot in slot_range.clone() {
                cluster_slots.push_back((
                    slot,
                    AtomicU64::new(0),
                    Arc::new(RwLock::new(map_maker())),
                ));
            }
        }
        // discard and recycle outdated elements
        loop {
            let (slot, _, _) = cluster_slots
                .front()
                .expect("After initialization the ring buffer can not be empty");
            // stop once we reach a slot in the valid range
            if *slot >= slot_range.start {
                break;
            }
            // pop useless record from the front
            let (_, _, map) = cluster_slots.pop_front().unwrap();
            // try to reuse its map allocation at the back of the datastructure
            let slot = cluster_slots.back().unwrap().0 + 1;
            let map = match Arc::try_unwrap(map) {
                Ok(map) => {
                    // map is free to reuse, reset all committed stakes to zero
                    // locking the map here is free since noone can be holding any locks
                    map.write()
                        .unwrap()
                        .values_mut()
                        .for_each(|v| v.store(0, Ordering::Relaxed));
                    map
                }
                // if we can not reuse just allocate a new one =(
                Err(_) => RwLock::new(map_maker()),
            };
            cluster_slots.push_back((slot, AtomicU64::new(0), Arc::new(map)));
        }
        debug_assert!(
            cluster_slots.len() == CLUSTER_SLOTS_TRIM_SIZE,
            "Ring buffer should be exactly the intended size"
        );
    }

    fn update_internal(
        &self,
        root: Slot,
        validator_stakes: &HashMap<Pubkey, u64>,
        epoch_slots_list: Vec<EpochSlots>,
    ) {
        let total_stake = self.total_stake.load(std::sync::atomic::Ordering::Relaxed);
        // Prepare a range of slots we will store in the datastructure
        // We will discard slots at or before current root or too far ahead.
        let slot_range = (root + 1)..root.saturating_add(CLUSTER_SLOTS_TRIM_SIZE as u64 + 1);
        // ensure the datastructure has the correct window in scope
        self.roll_cluster_slots(validator_stakes, &slot_range);

        let cluster_slots = self.cluster_slots.read().unwrap();
        for epoch_slots in epoch_slots_list {
            //filter out unstaked nodes
            let Some(&sender_stake) = validator_stakes.get(&epoch_slots.from) else {
                continue;
            };
            let updates = epoch_slots
                .to_slots(root)
                .filter(|slot| slot_range.contains(slot));
            // figure out which entries would get updated by the new message and cache them
            for slot in updates {
                let (s, slot_weight, map) = Self::_lookup(slot, &cluster_slots).unwrap();
                debug_assert_eq!(*s, slot, "Fetched slot does not match expected value!");
                let slot_weight_f64 = slot_weight.load(std::sync::atomic::Ordering::Relaxed) as f64;
                // once slot is confirmed beyond `DUPLICATE_THRESHOLD` we should not need
                // to update the stakes for it anymore
                // floats can be funny, give us a bit of margin
                if slot_weight_f64 / total_stake as f64 > FREEZE_THRESHOLD * 1.1 {
                    continue;
                }
                let rg = map.read().unwrap();
                let committed_stake = rg.get(&epoch_slots.from);
                // if there is already a record, we can atomic swap a correct value there
                if let Some(committed_stake) = committed_stake {
                    let old_stake = committed_stake.swap(sender_stake, Ordering::Relaxed);
                    if old_stake == 0 {
                        slot_weight.fetch_add(sender_stake, Ordering::Relaxed);
                    }
                } else {
                    drop(rg);
                    {
                        let mut wg = map.write().unwrap();
                        wg.insert(epoch_slots.from, AtomicU64::new(sender_stake));
                    }
                    self.total_writes.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Returns number of stored entries
    fn datastructure_size(&self) -> usize {
        let cluster_slots = self.cluster_slots.read().unwrap();
        cluster_slots
            .iter()
            .map(|(_slot, _confirmed, slot_pubkeys)| slot_pubkeys.read().unwrap().len())
            .sum::<usize>()
    }
    #[cfg(feature = "dev-context-only-utils")]
    pub fn generate_fill_for_tests(
        &self,
        stakes: &HashMap<Pubkey, u64>,
        root: Slot,
        slots: Range<Slot>,
    ) {
        self.total_stake.store(
            stakes.iter().map(|v| *v.1).sum(),
            std::sync::atomic::Ordering::Relaxed,
        );
        assert!(slots.start > root);
        let epochslots = make_epoch_slots(stakes, slots);
        self.update_internal(root, stakes, epochslots);
    }

    fn report_cluster_slots_size(&self) {
        if self.last_report.should_update(10_000) {
            let total_writes = self.total_writes.swap(0, Ordering::Relaxed);
            let total_allocations = self.total_allocations.swap(0, Ordering::Relaxed);
            datapoint_info!(
                "cluster-slots-size",
                ("total_entries", self.datastructure_size() as i64, i64),
                ("total_writes", total_writes as i64, i64),
                ("total_allocations", total_allocations as i64, i64),
            );
        }
    }

    #[cfg(test)]
    // patches the given node_id into the internal structures
    // to pretend as if it has submitted epoch slots for a given slot
    pub(crate) fn insert_node_id(&self, slot: Slot, node_id: Pubkey) {
        let Some(&balance) = self.validator_stakes.read().unwrap().get(&node_id) else {
            return;
        };
        if let Some(slot_pubkeys) = self.lookup(slot) {
            slot_pubkeys
                .write()
                .unwrap()
                .insert(node_id, AtomicU64::new(balance));
        } else {
            let mut cluster_slots = self.cluster_slots.write().unwrap();
            let mut hm = HashMap::default();
            hm.insert(node_id, AtomicU64::new(balance));
            cluster_slots.push_back((slot, AtomicU64::new(0), Arc::new(RwLock::new(hm))));
            cluster_slots.make_contiguous().sort_by_key(|(k, _, _)| *k);
        }
    }

    fn maybe_update_validator_stakes(
        &self,
        staked_nodes: &Arc<ValidatorStakesMap>,
        root_epoch: u64,
    ) {
        let my_epoch = *self.epoch.read().unwrap();
        if Some(root_epoch) != my_epoch {
            *self.validator_stakes.write().unwrap() = staked_nodes.clone();
            self.total_stake.store(
                staked_nodes.iter().map(|v| *v.1).sum(),
                std::sync::atomic::Ordering::Relaxed,
            );
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
            .map(|peer| {
                slot_peers
                    .get(peer.pubkey())
                    .map(|v| v.load(Ordering::Relaxed))
                    .unwrap_or(0)
            })
            .zip(stakes)
            .map(|(a, b)| (a / 2 + b / 2).max(1u64))
            .collect()
    }

    pub(crate) fn compute_weights_exclude_nonfrozen(
        &self,
        slot: Slot,
        repair_peers: &[ContactInfo],
    ) -> (Vec<u64>, Vec<usize>) {
        let Some(slot_peers) = self.lookup(slot) else {
            return (vec![], vec![]);
        };
        let mut weights = Vec::with_capacity(repair_peers.len());
        let mut indices = Vec::with_capacity(repair_peers.len());
        let slot_peers = slot_peers.read().unwrap();
        for (index, peer) in repair_peers.iter().enumerate() {
            if let Some(stake) = slot_peers.get(peer.pubkey()) {
                weights.push(stake.load(Ordering::Relaxed) + 1);
                indices.push(index);
            }
        }
        (weights, indices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default() {
        let cs = ClusterSlots::default();
        assert!(cs.cluster_slots.read().unwrap().is_empty());
    }

    #[test]
    fn test_update_noop() {
        let cs = ClusterSlots::default();
        cs.update_internal(0, &HashMap::new(), vec![]);
        let stored_slots = cs.datastructure_size();
        assert_eq!(stored_slots, 0);
    }

    #[test]
    fn test_update_empty() {
        let cs = ClusterSlots::default();
        let epoch_slot = EpochSlots::default();
        cs.update_internal(0, &HashMap::new(), vec![epoch_slot]);
        assert!(cs.lookup(0).is_none());
    }

    #[test]
    fn test_update_rooted() {
        //root is 0, so it should clear out the slot
        let cs = ClusterSlots::default();
        let mut epoch_slot = EpochSlots::default();
        epoch_slot.fill(&[0], 0);
        cs.update_internal(0, &HashMap::new(), vec![epoch_slot]);
        assert!(cs.lookup(0).is_none());
    }

    #[test]
    fn test_update_new_multiple_slots() {
        let cs = ClusterSlots::default();
        let mut epoch_slot1 = EpochSlots {
            from: Pubkey::new_unique(),
            ..Default::default()
        };
        epoch_slot1.fill(&[2, 4, 5], 0);
        let from1 = epoch_slot1.from;
        let mut epoch_slot2 = EpochSlots {
            from: Pubkey::new_unique(),
            ..Default::default()
        };
        epoch_slot2.fill(&[1, 3, 5], 1);
        let from2 = epoch_slot2.from;
        cs.update_internal(
            0,
            &HashMap::from([(from1, 10), (from2, 20)]),
            vec![epoch_slot1, epoch_slot2],
        );
        assert!(cs.lookup(0).is_none());
        assert!(cs.lookup(1).is_some());
        assert_eq!(
            cs.lookup(1).unwrap().read().unwrap()[&from2].load(Ordering::Relaxed),
            20
        );
        assert_eq!(
            cs.lookup(4).unwrap().read().unwrap()[&from1].load(Ordering::Relaxed),
            10
        );
        let binding = cs.lookup(5).unwrap();
        let map = binding.read().unwrap();
        assert_eq!(map[&from1].load(Ordering::Relaxed), 10);
        assert_eq!(map[&from2].load(Ordering::Relaxed), 20);
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
        let mut map = HashMap::default();
        let k1 = Pubkey::new_unique();
        let k2 = Pubkey::new_unique();
        map.insert(k1, AtomicU64::new(u64::MAX / 2));
        map.insert(k2, AtomicU64::new(1));
        cs.cluster_slots.write().unwrap().push_back((
            0,
            AtomicU64::new(0),
            Arc::new(RwLock::new(map)),
        ));
        let c1 = ContactInfo::new(k1, /*wallclock:*/ 0, /*shred_version:*/ 0);
        let c2 = ContactInfo::new(k2, /*wallclock:*/ 0, /*shred_version:*/ 0);
        assert_eq!(cs.compute_weights(0, &[c1, c2]), vec![u64::MAX / 4, 1]);
    }

    #[test]
    fn test_best_peer_3() {
        let cs = ClusterSlots::default();
        let mut map = HashMap::default();
        let k1 = solana_pubkey::new_rand();
        let k2 = solana_pubkey::new_rand();
        map.insert(k2, AtomicU64::new(1));
        cs.cluster_slots.write().unwrap().push_back((
            0,
            AtomicU64::new(0),
            Arc::new(RwLock::new(map)),
        ));
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
        let pk = Pubkey::new_unique();
        let map = vec![(pk, 1)].into_iter().collect();

        cs.update_internal(0, &map, vec![epoch_slot]);
        assert!(cs.lookup(1).is_some());
        assert_eq!(
            cs.lookup(1)
                .unwrap()
                .read()
                .unwrap()
                .get(&pk)
                .unwrap()
                .load(Ordering::Relaxed),
            1
        );
    }
}
