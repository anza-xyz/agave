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

pub type Stake = u64;

//This is intended to be switched to solana_pubkey::PubkeyHasherBuilder
type PubkeyHasherBuilder = RandomState;
pub(crate) type ValidatorStakesMap = HashMap<Pubkey, Stake, PubkeyHasherBuilder>;

/// Pubkey-stake map for nodes that have confirmed this slot.
/// If stake is zero the node has not confirmed the slot.
pub(crate) type SlotPubkeys =
    HashMap<Pubkey, AtomicU64 /* stake supporting */, PubkeyHasherBuilder>;

/// Amount of stake that needs to lock the slot for us to stop updating it.
/// This must be above `DUPLICATE_THRESHOLD` but also high enough to not starve
/// repair (as it will prefer nodes that have confirmed a slot)
const FREEZE_THRESHOLD: f64 = 0.9;
// whatever freeze threshold we should be above DUPLICATE_THRESHOLD in order to not break consensus.
// 1.1 margin is due to us not properly tracking epoch boundaries
static_assertions::const_assert!(FREEZE_THRESHOLD > DUPLICATE_THRESHOLD * 1.1);

#[derive(Default)]
pub struct ClusterSlots {
    // ring buffer storing, per slot, which stakes were committed to a certain slot.
    cluster_slots: RwLock<VecDeque<RowContent>>,
    // a cache of validator stakes for reuse internally, updated at epoch boundary.
    validator_stakes: RwLock<Arc<ValidatorStakesMap>>,
    total_stake: AtomicU64, // total amount of stake across all validators in validator_stakes.
    current_slot: AtomicU64, // current slot at the front of ringbuffer.
    epoch: RwLock<Option<u64>>, //current epoch.
    cursor: Mutex<Cursor>,  // cursor to read CRDS.
    metrics_last_report: AtomicInterval, // last time statistics were reported.
    metric_allocations: AtomicU64, // total amount of memory allocations made.
    metric_write_locks: AtomicU64, // total amount of write locks taken outside of initialization.
}

#[derive(Debug)]
struct RowContent {
    slot: Slot,               // slot for which this row stores information
    total_support: AtomicU64, // total committed stake for this slot
    supporters: Arc<RwLock<SlotPubkeys>>,
}

impl ClusterSlots {
    #[inline]
    pub(crate) fn lookup(&self, slot: Slot) -> Option<Arc<RwLock<SlotPubkeys>>> {
        let cluster_slots = self.cluster_slots.read().unwrap();
        Some(
            Self::get_row_for_slot(slot, &cluster_slots)?
                .supporters
                .clone(),
        )
    }

    #[inline]
    fn get_row_for_slot(slot: Slot, cluster_slots: &VecDeque<RowContent>) -> Option<&RowContent> {
        let start = cluster_slots.front()?.slot;
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
        let current_slot = self.current_slot.load(Ordering::Relaxed);
        if current_slot > root_slot {
            error!("Invalid update call to ClusterSlots, can not roll time backwards!");
            return;
        }
        self.maybe_update_validator_stakes(validator_stakes, root_epoch);
        let epoch_slots = {
            let mut cursor = self.cursor.lock().unwrap();
            cluster_info.get_epoch_slots(&mut cursor)
        };
        self.update_internal(root_slot, validator_stakes, epoch_slots);
        self.report_cluster_slots_perf_stats();
    }

    /// Advance the cluster_slots ringbuffer, initialize if needed.
    /// We will discard slots at or before current root or too far ahead.
    fn roll_cluster_slots(
        &self,
        validator_stakes: &HashMap<Pubkey, u64>,
        root: Slot,
    ) -> Range<Slot> {
        let slot_range = (root + 1)..root.saturating_add(CLUSTER_SLOTS_TRIM_SIZE as u64 + 1);
        let current_slot = self.current_slot.swap(slot_range.start, Ordering::Relaxed);
        // early-return if no slot change happened
        if current_slot == slot_range.start {
            return slot_range;
        }
        assert!(
            slot_range.start > current_slot,
            "Can not roll cluster slots backwards!"
        );
        let map_capacity = validator_stakes.len();
        let mut cluster_slots = self.cluster_slots.write().unwrap();
        self.metric_write_locks.fetch_add(1, Ordering::Relaxed);
        // zero-initialize a given map
        let zero_init_map = |map: &mut SlotPubkeys| {
            map.extend(
                validator_stakes
                    .iter()
                    .map(|(k, _v)| (*k, AtomicU64::new(0))),
            );
        };
        // a handy closure to spawn new hashmaps with the correct parameters
        let map_maker = || {
            let mut map =
                HashMap::with_capacity_and_hasher(map_capacity, PubkeyHasherBuilder::default());
            zero_init_map(&mut map);
            self.metric_allocations.fetch_add(1, Ordering::Relaxed);
            map
        };
        //startup init, this is very slow but only ever happens once
        if cluster_slots.is_empty() {
            for slot in slot_range.clone() {
                cluster_slots.push_back(RowContent {
                    slot,
                    total_support: AtomicU64::new(0),
                    supporters: Arc::new(RwLock::new(map_maker())),
                });
            }
        }
        // discard and recycle outdated elements
        loop {
            let RowContent { slot, .. } = cluster_slots
                .front()
                .expect("After initialization the ring buffer can not be empty");
            // stop once we reach a slot in the valid range
            if *slot >= slot_range.start {
                break;
            }
            // pop useless record from the front
            let RowContent {
                supporters: map, ..
            } = cluster_slots.pop_front().unwrap();
            // try to reuse its map allocation at the back of the datastructure
            let slot = cluster_slots.back().unwrap().slot + 1;
            let map = match Arc::try_unwrap(map) {
                Ok(map) => {
                    // map is free to reuse, reset its content reusing memory
                    // locking the map here is free since noone can be holding any locks
                    let mut wg = map.write().unwrap();
                    if wg.capacity() > validator_stakes.len() * 2 {
                        // map is too large for what we need currently, reallocate it
                        *wg = map_maker();
                    } else {
                        // reinit the existing memory with up-to-date validator set
                        wg.clear();
                        zero_init_map(&mut wg);
                    }
                    drop(wg);
                    map
                }
                // if we can not reuse just allocate a new one =(
                Err(_) => RwLock::new(map_maker()),
            };
            cluster_slots.push_back(RowContent {
                slot,
                total_support: AtomicU64::new(0),
                supporters: Arc::new(map),
            });
        }
        debug_assert!(
            cluster_slots.len() == CLUSTER_SLOTS_TRIM_SIZE,
            "Ring buffer should be exactly the intended size"
        );
        slot_range
    }

    fn update_internal(
        &self,
        root: Slot,
        validator_stakes: &HashMap<Pubkey, u64>,
        epoch_slots_list: Vec<EpochSlots>,
    ) {
        let total_stake = self.total_stake.load(std::sync::atomic::Ordering::Relaxed);
        // Adjust the range of slots we can store in the datastructure to the
        // current rooted slot, ensure the datastructure has the correct window in scope
        let slot_range = self.roll_cluster_slots(validator_stakes, root);

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
                let RowContent {
                    slot: s,
                    total_support,
                    supporters: map,
                } = Self::get_row_for_slot(slot, &cluster_slots).unwrap();
                debug_assert_eq!(*s, slot, "Fetched slot does not match expected value!");
                let slot_weight_f64 =
                    total_support.load(std::sync::atomic::Ordering::Relaxed) as f64;
                // once slot is confirmed beyond `FREEZE_THRESHOLD` we should not need
                // to update the stakes for it anymore
                if slot_weight_f64 / total_stake as f64 > FREEZE_THRESHOLD {
                    continue;
                }
                let rg = map.read().unwrap();
                let committed_stake = rg.get(&epoch_slots.from);
                // if there is already a record, we can atomic swap a correct value there
                if let Some(committed_stake) = committed_stake {
                    let old_stake = committed_stake.swap(sender_stake, Ordering::Relaxed);
                    if old_stake == 0 {
                        total_support.fetch_add(sender_stake, Ordering::Relaxed);
                    }
                } else {
                    drop(rg);
                    {
                        let mut wg = map.write().unwrap();
                        wg.insert(epoch_slots.from, AtomicU64::new(sender_stake));
                    }
                    self.metric_write_locks.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Returns number of stored entries
    fn datastructure_size(&self) -> usize {
        let cluster_slots = self.cluster_slots.read().unwrap();
        cluster_slots
            .iter()
            .map(|RowContent { supporters, .. }| supporters.read().unwrap().len())
            .sum::<usize>()
    }

    fn report_cluster_slots_perf_stats(&self) {
        if self.metrics_last_report.should_update(10_000) {
            let write_locks = self.metric_write_locks.swap(0, Ordering::Relaxed);
            let allocations = self.metric_allocations.swap(0, Ordering::Relaxed);
            datapoint_info!(
                "cluster-slots-size",
                ("total_entries", self.datastructure_size() as i64, i64),
                ("write_locks", write_locks as i64, i64),
                ("total_allocations", allocations as i64, i64),
            );
        }
    }

    #[cfg(test)]
    // patches the given node_id into the internal structures
    // to pretend as if it has submitted epoch slots for a given slot.
    // If the node was not previosly registered in validator_stakes,
    // an override_stake amount should be provided.
    pub(crate) fn insert_node_id(
        &self,
        slot: Slot,
        node_id: Pubkey,
        override_stake: Option<Stake>,
    ) {
        let balance = if let Some(stake) = override_stake {
            let mut stakes = self.validator_stakes.read().unwrap().as_ref().clone();
            stakes.insert(node_id, stake);
            self.update_total_stake(stakes.values().cloned());
            *self.validator_stakes.write().unwrap() = Arc::new(stakes);
            stake
        } else {
            *self.validator_stakes.read().unwrap().get(&node_id).expect(
                "If the node is not registered, override_stake should be supplied to set its stake",
            )
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
            cluster_slots.push_back(RowContent {
                slot,
                total_support: AtomicU64::new(0),
                supporters: Arc::new(RwLock::new(hm)),
            });
            cluster_slots
                .make_contiguous()
                .sort_by_key(|RowContent { slot, .. }| *slot);
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
            self.update_total_stake(staked_nodes.values().cloned());
            *self.epoch.write().unwrap() = Some(root_epoch);
        }
    }

    fn update_total_stake(&self, stakes: impl IntoIterator<Item = u64>) {
        self.total_stake
            .store(stakes.into_iter().sum(), Ordering::Relaxed);
    }

    pub(crate) fn compute_weights(&self, slot: Slot, repair_peers: &[ContactInfo]) -> Vec<u64> {
        if repair_peers.is_empty() {
            return vec![];
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
    fn test_roll_cluster_slots() {
        let cs = ClusterSlots::default();
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();

        let trimsize = CLUSTER_SLOTS_TRIM_SIZE as u64;
        let validator_stakes = HashMap::from([(pk1, 10), (pk2, 20)]);
        assert_eq!(
            cs.cluster_slots.read().unwrap().len(),
            0,
            "ring should be initially empty"
        );
        cs.roll_cluster_slots(&validator_stakes, 0);
        {
            let rg = cs.cluster_slots.read().unwrap();
            assert_eq!(
                rg.len(),
                CLUSTER_SLOTS_TRIM_SIZE,
                "ring should have exactly {} elements",
                CLUSTER_SLOTS_TRIM_SIZE
            );
            assert_eq!(rg.front().unwrap().slot, 1, "first slot should be root + 1");
            assert_eq!(
                rg.back().unwrap().slot - rg.front().unwrap().slot,
                trimsize - 1,
                "ring should have the right size"
            );
        }
        //step 1 slot
        cs.roll_cluster_slots(&validator_stakes, 1);
        {
            let rg = cs.cluster_slots.read().unwrap();
            assert_eq!(rg.front().unwrap().slot, 2, "first slot should be root + 1");
            assert_eq!(
                rg.back().unwrap().slot - rg.front().unwrap().slot,
                trimsize - 1,
                "ring should have the right size"
            );
        }
        let allocs = cs.metric_allocations.load(Ordering::Relaxed);
        // make 1 full loop
        cs.roll_cluster_slots(&validator_stakes, trimsize);
        {
            let rg = cs.cluster_slots.read().unwrap();
            assert_eq!(
                rg.front().unwrap().slot,
                trimsize + 1,
                "first slot should be root + 1"
            );
            let allocs = cs.metric_allocations.load(Ordering::Relaxed) - allocs;
            assert_eq!(allocs, 0, "No need to allocate when rolling ringbuf");
            assert_eq!(
                rg.back().unwrap().slot - rg.front().unwrap().slot,
                trimsize - 1,
                "ring should have the right size"
            );
        }
    }

    #[test]
    #[should_panic]
    fn test_roll_cluster_slots_backwards() {
        let cs = ClusterSlots::default();
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();

        let validator_stakes = HashMap::from([(pk1, 10), (pk2, 20)]);
        cs.roll_cluster_slots(&validator_stakes, 10);
        cs.roll_cluster_slots(&validator_stakes, 5);
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
        //root is 0, so it should be a noop
        let cs = ClusterSlots::default();
        let mut epoch_slot = EpochSlots::default();
        epoch_slot.fill(&[0], 0);
        cs.update_internal(0, &HashMap::new(), vec![epoch_slot]);
        assert!(cs.lookup(0).is_none());
    }

    #[test]
    fn test_update_multiple_slots() {
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
        cs.update_total_stake([1000]); //disable slot locking
        cs.update_internal(
            0,
            &HashMap::from([(from1, 10), (from2, 20)]),
            vec![epoch_slot1, epoch_slot2],
        );
        assert!(
            cs.lookup(0).is_none(),
            "slot 0 should not be supported by anyone"
        );
        assert!(cs.lookup(1).is_some(), "slot 1 should be supported");
        assert_eq!(
            cs.lookup(1).unwrap().read().unwrap()[&from2].load(Ordering::Relaxed),
            20,
            "support should come from validator 2"
        );
        assert_eq!(
            cs.lookup(4).unwrap().read().unwrap()[&from1].load(Ordering::Relaxed),
            10,
            "validator 1 should support slot 4"
        );
        let binding = cs.lookup(5).unwrap();
        let map = binding.read().unwrap();
        assert_eq!(
            map[&from1].load(Ordering::Relaxed),
            10,
            "both should support slot 5"
        );
        assert_eq!(
            map[&from2].load(Ordering::Relaxed),
            20,
            "both should support slot 5"
        );
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
        cs.cluster_slots.write().unwrap().push_back(RowContent {
            slot: 0,
            total_support: AtomicU64::new(0),
            supporters: Arc::new(RwLock::new(map)),
        });
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
        cs.cluster_slots.write().unwrap().push_back(RowContent {
            slot: 0,
            total_support: AtomicU64::new(0),
            supporters: Arc::new(RwLock::new(map)),
        });
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
        let validator_stakes: HashMap<_, _> = vec![
            (*contact_infos[0].pubkey(), 42),
            (*contact_infos[1].pubkey(), u64::MAX / 2),
        ]
        .into_iter()
        .collect();
        *cs.validator_stakes.write().unwrap() = Arc::new(validator_stakes);

        // Mark the first validator as completed slot 9, should pick that validator,
        // even though it only has minimal stake, while the other validator has
        // max stake
        cs.insert_node_id(slot, *contact_infos[0].pubkey(), None);
        let (w, i) = cs.compute_weights_exclude_nonfrozen(slot, &contact_infos);
        assert_eq!(w, [43]);
        assert_eq!(i, [0]);
    }

    #[test]
    fn test_update_new_staked_slot() {
        let cs = ClusterSlots::default();
        let pk = Pubkey::new_unique();
        let mut epoch_slot = EpochSlots {
            from: pk,
            ..Default::default()
        };
        epoch_slot.fill(&[1], 0);
        let map = HashMap::from([(pk, 42)]);

        cs.update_internal(0, &map, vec![epoch_slot]);
        assert!(cs.lookup(1).is_some(), "slot 1 should have records");
        assert_eq!(
            cs.lookup(1)
                .unwrap()
                .read()
                .unwrap()
                .get(&pk)
                .unwrap()
                .load(Ordering::Relaxed),
            42,
            "the stake of the node should be commited to the slot"
        );
    }
}
