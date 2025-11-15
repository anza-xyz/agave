/// ClusterSlots object holds information about which validators have confirmed which slots
/// via EpochSlots mechanism. Periodically, EpochSlots get sent into here via update method.
/// The ClusterSlots datastructure maintains a shadow copy of the stake info for current and
/// upcoming epochs, and a ringbuffer of per-slot records. The ringbuffer may contain blank
/// records if stake information is not available (can happen for very short epochs).
use {
    crate::{cluster_slots_service::slot_supporters::SlotSupporters, consensus::Stake},
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::{
        cluster_info::ClusterInfo, contact_info::ContactInfo, crds::Cursor, epoch_slots::EpochSlots,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::VersionedEpochStakes},
    solana_time_utils::AtomicInterval,
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

//This is intended to be switched to solana_pubkey::PubkeyHasherBuilder
type PubkeyHasherBuilder = RandomState;
pub(crate) type ValidatorStakesMap = HashMap<Pubkey, Stake, PubkeyHasherBuilder>;

/// Static snapshot of the information about a given epoch's stake distribution.
struct EpochStakeInfo {
    validator_stakes: Arc<ValidatorStakesMap>,
    pubkey_to_index: Arc<HashMap<Pubkey, usize>>,
    /// total amount of stake across all validators in `validator_stakes`.
    total_stake: Stake,
}

impl From<&VersionedEpochStakes> for EpochStakeInfo {
    fn from(stakes: &VersionedEpochStakes) -> Self {
        let validator_stakes = ValidatorStakesMap::from_iter(
            stakes
                .node_id_to_vote_accounts()
                .iter()
                .map(|(k, v)| (*k, v.total_stake)),
        );
        Self::new(validator_stakes, stakes.total_stake())
    }
}

impl EpochStakeInfo {
    fn new(validator_stakes: HashMap<Pubkey, Stake>, total_stake: Stake) -> Self {
        let pubkey_to_index: HashMap<Pubkey, usize, PubkeyHasherBuilder> = validator_stakes
            .keys()
            .enumerate()
            .map(|(v, &k)| (k, v))
            .collect();
        EpochStakeInfo {
            validator_stakes: Arc::new(validator_stakes),
            pubkey_to_index: Arc::new(pubkey_to_index),
            total_stake,
        }
    }
}

/// Holds schedule of the epoch where root bank currently sits
struct RootEpoch {
    number: Epoch,
    schedule: EpochSchedule,
}

pub struct ClusterSlots {
    // ring buffer storing, per slot, which stakes were committed to a certain slot.
    cluster_slots: RwLock<VecDeque<RowContent>>,
    // a cache of validator stakes for reuse internally, updated at epoch boundary.
    epoch_metadata: RwLock<HashMap<Epoch, EpochStakeInfo>>,
    current_slot: AtomicU64, // current slot at the front of ringbuffer.
    root_epoch: RwLock<Option<RootEpoch>>, // epoch where root bank is
    cursor: Mutex<Cursor>,   // cursor to read CRDS.
    metrics_last_report: AtomicInterval, // last time statistics were reported.
    metric_allocations: AtomicU64, // total amount of memory allocations made.
    metric_write_locks: AtomicU64, // total amount of write locks taken outside of initialization.
}

#[derive(Debug)]
struct RowContent {
    slot: Slot, // slot for which this row stores information
    supporters: Arc<SlotSupporters>,
}

impl ClusterSlots {
    pub fn new(root_bank: &Bank, cluster_info: &ClusterInfo) -> Self {
        let cluster_slots = Self::default();
        cluster_slots.update(root_bank, cluster_info);
        cluster_slots
    }

    // Intentionally private default function to disallow uninitialized construction
    fn default() -> Self {
        Self {
            cluster_slots: RwLock::new(VecDeque::new()),
            epoch_metadata: RwLock::new(HashMap::new()),
            current_slot: AtomicU64::default(),
            root_epoch: RwLock::new(None),
            cursor: Mutex::new(Cursor::default()),
            metrics_last_report: AtomicInterval::default(),
            metric_allocations: AtomicU64::default(),
            metric_write_locks: AtomicU64::default(),
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn default_for_tests() -> Self {
        Self::default()
    }

    #[inline]
    pub(crate) fn lookup(&self, slot: Slot) -> Option<Arc<SlotSupporters>> {
        let cluster_slots = self.cluster_slots.read().unwrap();

        let row = Self::get_row_for_slot(slot, &cluster_slots)?;
        if row.supporters.is_blank() {
            None
        } else {
            Some(row.supporters.clone())
        }
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

    #[inline]
    fn get_row_for_slot_mut(
        slot: Slot,
        cluster_slots: &mut VecDeque<RowContent>,
    ) -> Option<&mut RowContent> {
        let start = cluster_slots.front()?.slot;
        if slot < start {
            return None;
        }
        let idx = slot - start;
        cluster_slots.get_mut(idx as usize)
    }

    pub(crate) fn update(&self, root_bank: &Bank, cluster_info: &ClusterInfo) {
        let root_slot = root_bank.slot();
        let current_slot = self.get_current_slot();
        if current_slot > root_slot + 1 {
            error!("Invalid update call to ClusterSlots, can not roll time backwards!");
            return;
        }
        let root_epoch = root_bank.epoch();
        if self.need_to_update_epoch(root_epoch) {
            self.update_epoch_info(root_bank);
        }

        let epoch_slots = {
            let mut cursor = self.cursor.lock().unwrap();
            cluster_info.get_epoch_slots(&mut cursor)
        };
        self.update_internal(root_slot, epoch_slots);
        self.maybe_report_cluster_slots_perf_stats();
    }

    fn need_to_update_epoch(&self, root_epoch: Epoch) -> bool {
        let rg = self.root_epoch.read().unwrap();
        let my_epoch = rg.as_ref().map(|v| v.number);
        Some(root_epoch) != my_epoch
    }

    // call this to update internal datastructures for current and next epoch
    fn update_epoch_info(&self, root_bank: &Bank) {
        let root_epoch = root_bank.epoch();
        info!("Updating epoch_metadata for epoch {root_epoch}");
        let epoch_stakes_map = root_bank.epoch_stakes_map();

        {
            let mut epoch_metadata = self.epoch_metadata.write().unwrap();
            // check if we need to do any cleanup in the epoch_metadata
            {
                if let Some(my_epoch) = self.get_epoch_for_slot(self.get_current_slot()) {
                    info!("Evicting epoch_metadata for epoch {my_epoch}");
                    epoch_metadata.remove(&my_epoch);
                }
            }
            // Next we fetch info about current and upcoming epoch's stakes
            epoch_metadata.insert(
                root_epoch,
                EpochStakeInfo::from(&epoch_stakes_map[&root_epoch]),
            );
        }
        *self.root_epoch.write().unwrap() = Some(RootEpoch {
            schedule: root_bank.epoch_schedule().clone(),
            number: root_epoch,
        });

        let next_epoch = root_epoch.wrapping_add(1);
        let next_epoch_info = EpochStakeInfo::from(&epoch_stakes_map[&next_epoch]);
        let mut first_slot = root_bank
            .epoch_schedule()
            .get_first_slot_in_epoch(next_epoch);
        let mut cluster_slots = self.cluster_slots.write().unwrap();
        let mut patched = 0;
        loop {
            let row = Self::get_row_for_slot_mut(first_slot, &mut cluster_slots);
            let Some(row) = row else {
                break; // reached the end of ringbuffer and/or ringbuffer is not initialized
            };
            // rows for this epoch are not initialized, initialize them now
            if row.supporters.is_blank() {
                patched += 1;
                row.supporters = Arc::new(SlotSupporters::new(
                    next_epoch_info.total_stake,
                    next_epoch_info.pubkey_to_index.clone(),
                ));
            } else {
                // if any rows for this epoch are initialized, they all should be
                break;
            }
            first_slot = first_slot.wrapping_add(1);
        }
        if patched > 0 {
            warn!("Finalized init for {patched} slots in epoch {next_epoch}");
        }
        let mut epoch_metadata = self.epoch_metadata.write().unwrap();
        epoch_metadata.insert(next_epoch, next_epoch_info);
    }
    #[cfg(test)]
    pub(crate) fn fake_epoch_info_for_tests(&self, validator_stakes: ValidatorStakesMap) {
        assert!(
            self.root_epoch.read().unwrap().is_none(),
            "Can not use fake epoch initialization more than once!"
        );
        let sched = EpochSchedule::without_warmup();
        *self.root_epoch.write().unwrap() = Some(RootEpoch {
            schedule: sched,
            number: 0,
        });
        let total_stake = validator_stakes.values().sum();
        let mut epoch_metadata = self.epoch_metadata.write().unwrap();
        epoch_metadata.insert(0, EpochStakeInfo::new(validator_stakes, total_stake));
    }

    /// Advance the cluster_slots ringbuffer, initialize if needed.
    /// We will discard slots at or before current root or too far ahead.
    fn roll_cluster_slots(&self, root: Slot) -> Range<Slot> {
        let slot_range = (root + 1)..root.saturating_add(CLUSTER_SLOTS_TRIM_SIZE as u64 + 1);
        let current_slot = self.current_slot.load(Ordering::Relaxed);
        // early-return if no slot change happened
        if current_slot == slot_range.start {
            return slot_range;
        }
        assert!(
            slot_range.start > current_slot,
            "Can not roll cluster slots backwards!"
        );
        let mut cluster_slots = self.cluster_slots.write().unwrap();
        self.metric_write_locks.fetch_add(1, Ordering::Relaxed);
        let epoch_metadata = self.epoch_metadata.read().unwrap();
        //startup init, this is very slow but only ever happens once
        if cluster_slots.is_empty() {
            info!("Init cluster_slots at range {slot_range:?}");
            for slot in slot_range.clone() {
                // Epoch should be defined for all slots in the window
                let epoch = self
                    .get_epoch_for_slot(slot)
                    .expect("Epoch should be defined for all slots");
                let supporters = if let Some(epoch_data) = epoch_metadata.get(&epoch) {
                    SlotSupporters::new(epoch_data.total_stake, epoch_data.pubkey_to_index.clone())
                } else {
                    // we should be able to initialize at least current epoch right away
                    if epoch
                        == self
                            .get_epoch_for_slot(current_slot)
                            .expect("Epochs should be defined")
                    {
                        panic!(
                            "Epoch slots can not find stake info for slot {slot} in epoch {epoch}"
                        );
                    }
                    SlotSupporters::new_blank()
                };
                self.metric_allocations.fetch_add(1, Ordering::Relaxed);
                cluster_slots.push_back(RowContent {
                    slot,
                    supporters: Arc::new(supporters),
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
            let RowContent { supporters, .. } = cluster_slots.pop_front().unwrap();
            // try to reuse its map allocation at the back of the datastructure
            let slot = cluster_slots.back().unwrap().slot + 1;

            let epoch = self
                .get_epoch_for_slot(slot)
                .expect("Epoch should be defined for all slots in the window");
            let Some(stake_info) = epoch_metadata.get(&epoch) else {
                warn!(
                    "Epoch slots can not reuse slot entry for slot {slot} since stakes for epoch \
                     {epoch} are not available"
                );
                cluster_slots.push_back(RowContent {
                    slot,
                    supporters: Arc::new(SlotSupporters::new_blank()),
                });
                break;
            };

            let new_supporters = match Arc::try_unwrap(supporters) {
                Ok(supporters) => {
                    supporters.recycle(stake_info.total_stake, &stake_info.pubkey_to_index)
                }
                // if we can not reuse just allocate a new one =(
                Err(_) => {
                    self.metric_allocations.fetch_add(1, Ordering::Relaxed);
                    SlotSupporters::new(stake_info.total_stake, stake_info.pubkey_to_index.clone())
                }
            };
            cluster_slots.push_back(RowContent {
                slot,
                supporters: Arc::new(new_supporters),
            });
        }
        debug_assert!(
            cluster_slots.len() == CLUSTER_SLOTS_TRIM_SIZE,
            "Ring buffer should be exactly the intended size"
        );
        self.current_slot.store(slot_range.start, Ordering::Relaxed);
        slot_range
    }

    fn update_internal(&self, root: Slot, epoch_slots_list: Vec<EpochSlots>) {
        // Adjust the range of slots we can store in the datastructure to the
        // current rooted slot, ensure the datastructure has the correct window in scope
        let slot_range = self.roll_cluster_slots(root);

        let epoch_metadata = self.epoch_metadata.read().unwrap();
        let cluster_slots = self.cluster_slots.read().unwrap();
        for epoch_slots in epoch_slots_list {
            let Some(first_slot) = epoch_slots.first_slot() else {
                continue;
            };
            let Some(epoch) = self.get_epoch_for_slot(first_slot) else {
                continue;
            };
            let Some(epoch_meta) = epoch_metadata.get(&epoch) else {
                continue;
            };
            //filter out unstaked nodes
            let Some(&sender_stake) = epoch_meta.validator_stakes.get(&epoch_slots.from) else {
                continue;
            };
            let updates = epoch_slots
                .to_slots(root)
                .filter(|slot| slot_range.contains(slot));
            // figure out which entries would get updated by the new message and cache them
            for slot in updates {
                let RowContent {
                    slot: s,
                    supporters: map,
                } = Self::get_row_for_slot(slot, &cluster_slots).unwrap();
                debug_assert_eq!(*s, slot, "Fetched slot does not match expected value!");
                if map.is_frozen() {
                    continue;
                }
                if map
                    .set_support_by_pubkey(&epoch_slots.from, sender_stake)
                    .is_err()
                {
                    error!("Unexpected pubkey {} for slot {}!", &epoch_slots.from, slot);
                    break;
                }
            }
        }
    }

    /// Upload stats into metrics database
    fn maybe_report_cluster_slots_perf_stats(&self) {
        if self.metrics_last_report.should_update(10_000) {
            let write_locks = self.metric_write_locks.swap(0, Ordering::Relaxed);
            let allocations = self.metric_allocations.swap(0, Ordering::Relaxed);
            let cluster_slots = self.cluster_slots.read().unwrap();
            let (size, frozen, blank) =
                cluster_slots
                    .iter()
                    .fold((0, 0, 0), |(s, f, b), RowContent { supporters, .. }| {
                        (
                            s + supporters.memory_usage(),
                            f + supporters.is_frozen() as usize,
                            b + supporters.is_blank() as usize,
                        )
                    });
            datapoint_info!(
                "cluster-slots-size",
                ("total_entries", size as i64, i64),
                ("frozen_entries", frozen as i64, i64),
                ("blank_entries", blank as i64, i64),
                ("write_locks", write_locks as i64, i64),
                ("total_allocations", allocations as i64, i64),
            );
        }
    }
    fn get_current_slot(&self) -> Slot {
        self.current_slot.load(Ordering::Relaxed)
    }

    fn with_root_epoch<T>(&self, closure: impl FnOnce(&RootEpoch) -> T) -> Option<T> {
        let rg = self.root_epoch.read().unwrap();
        rg.as_ref().map(closure)
    }

    fn get_epoch_for_slot(&self, slot: Slot) -> Option<u64> {
        self.with_root_epoch(|b| b.schedule.get_epoch_and_slot_index(slot).0)
    }

    #[cfg(test)]
    // patches the given node_id into the internal structures
    // to pretend as if it has submitted epoch slots for a given slot.
    // If the node was not previously registered in validator_stakes,
    // an override_stake amount should be provided.
    pub(crate) fn insert_node_id(&self, slot: Slot, node_id: Pubkey) {
        let mut epoch_slot = EpochSlots {
            from: node_id,
            ..Default::default()
        };
        epoch_slot.fill(&[slot], 0);
        let current_root = self.current_slot.load(Ordering::Relaxed);
        self.update_internal(current_root, vec![epoch_slot]);
    }

    pub(crate) fn compute_weights(&self, slot: Slot, repair_peers: &[ContactInfo]) -> Vec<u64> {
        if repair_peers.is_empty() {
            return vec![];
        }
        let stakes = {
            let failsafe = std::iter::repeat_n(1, repair_peers.len());
            let Some(epoch) = self.get_epoch_for_slot(slot) else {
                error!("No epoch info for slot {slot}");
                return Vec::from_iter(failsafe);
            };
            let epoch_metadata = self.epoch_metadata.read().unwrap();
            let Some(stakeinfo) = epoch_metadata.get(&epoch) else {
                error!("No epoch_metadata record for epoch {epoch}");
                return Vec::from_iter(failsafe);
            };
            let validator_stakes = stakeinfo.validator_stakes.as_ref();
            repair_peers
                .iter()
                .map(|peer| {
                    validator_stakes
                        .get(peer.pubkey())
                        .cloned()
                        .unwrap_or(1)
                        .max(1)
                })
                .collect()
        };
        let Some(slot_peers) = self.lookup(slot) else {
            return stakes;
        };

        repair_peers
            .iter()
            .map(|peer| slot_peers.get_support_by_pubkey(peer.pubkey()).unwrap_or(0))
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

        for (index, peer) in repair_peers.iter().enumerate() {
            if let Some(stake) = slot_peers.get_support_by_pubkey(peer.pubkey()) {
                if stake > 0 {
                    weights.push(stake.max(1));
                    indices.push(index);
                }
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
        cs.fake_epoch_info_for_tests(validator_stakes);
        cs.roll_cluster_slots(0);
        {
            let rg = cs.cluster_slots.read().unwrap();
            assert_eq!(
                rg.len(),
                CLUSTER_SLOTS_TRIM_SIZE,
                "ring should have exactly {CLUSTER_SLOTS_TRIM_SIZE} elements"
            );
            assert_eq!(rg.front().unwrap().slot, 1, "first slot should be root + 1");
            assert_eq!(
                rg.back().unwrap().slot - rg.front().unwrap().slot,
                trimsize - 1,
                "ring should have the right size"
            );
        }
        //step 1 slot
        cs.roll_cluster_slots(1);
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
        cs.roll_cluster_slots(trimsize);
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

    fn fake_stakes() -> (Pubkey, Pubkey, ValidatorStakesMap) {
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();

        let validator_stakes = HashMap::from([(pk1, 10), (pk2, 20)]);
        (pk1, pk2, validator_stakes)
    }

    #[test]
    #[should_panic]
    fn test_roll_cluster_slots_backwards() {
        let cs = ClusterSlots::default();
        let (_, _, validator_stakes) = fake_stakes();

        cs.fake_epoch_info_for_tests(validator_stakes);
        cs.roll_cluster_slots(10);
        cs.roll_cluster_slots(5);
    }

    #[test]
    fn test_update_empty() {
        let cs = ClusterSlots::default();
        let (pk1, _, validator_stakes) = fake_stakes();
        cs.fake_epoch_info_for_tests(validator_stakes);
        let epoch_slot = EpochSlots {
            from: pk1,
            ..Default::default()
        };
        cs.update_internal(0, vec![epoch_slot]);
        assert!(cs.lookup(0).is_none());
    }

    #[test]
    fn test_update_rooted() {
        //root is 0, so it should be a noop
        let cs = ClusterSlots::default();
        let (pk1, _, validator_stakes) = fake_stakes();
        cs.fake_epoch_info_for_tests(validator_stakes);
        let mut epoch_slot = EpochSlots {
            from: pk1,
            ..Default::default()
        };
        epoch_slot.fill(&[0], 0);
        cs.update_internal(0, vec![epoch_slot]);
        assert!(cs.lookup(0).is_none());
    }

    #[test]
    fn test_update_multiple_slots() {
        let cs = ClusterSlots::default();
        let (pk1, pk2, validator_stakes) = fake_stakes();
        cs.fake_epoch_info_for_tests(validator_stakes);

        let mut epoch_slot1 = EpochSlots {
            from: pk1,
            ..Default::default()
        };
        epoch_slot1.fill(&[2, 4, 5], 0);
        let mut epoch_slot2 = EpochSlots {
            from: pk2,
            ..Default::default()
        };
        epoch_slot2.fill(&[1, 3, 5], 1);
        cs.update_internal(0, vec![epoch_slot1, epoch_slot2]);
        assert!(
            cs.lookup(0).is_none(),
            "slot 0 should not be supported by anyone"
        );
        assert!(cs.lookup(1).is_some(), "slot 1 should be supported");
        assert_eq!(
            cs.lookup(1).unwrap().get_support_by_pubkey(&pk2),
            Some(20),
            "support should come from validator 2"
        );
        assert_eq!(
            cs.lookup(4).unwrap().get_support_by_pubkey(&pk1),
            Some(10),
            "validator 1 should support slot 4"
        );
        let map = cs.lookup(5).unwrap();
        assert_eq!(
            map.get_support_by_pubkey(&pk1),
            Some(10),
            "both should support slot 5"
        );
        assert_eq!(
            map.get_support_by_pubkey(&pk2),
            Some(20),
            "both should support slot 5"
        );
    }

    #[test]
    fn test_compute_weights_failsafes() {
        let cs = ClusterSlots::default();
        let ci = ContactInfo::default();
        assert_eq!(cs.compute_weights(0, &[ci]), vec![1]);

        let (_, _, validator_stakes) = fake_stakes();
        cs.fake_epoch_info_for_tests(validator_stakes);
        let ci = ContactInfo::default();
        assert_eq!(cs.compute_weights(0, &[ci]), vec![1]);
    }

    #[test]
    fn test_best_peer_2() {
        let cs = ClusterSlots::default();
        let mut map = HashMap::default();
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        map.insert(pk1, 1000);
        map.insert(pk2, 10);
        map.insert(Pubkey::new_unique(), u64::MAX / 2);
        cs.fake_epoch_info_for_tests(map);

        let mut epoch_slot1 = EpochSlots {
            from: pk1,
            ..Default::default()
        };
        epoch_slot1.fill(&[1, 2, 3, 4], 0);
        let mut epoch_slot2 = EpochSlots {
            from: pk2,
            ..Default::default()
        };
        epoch_slot2.fill(&[1, 2, 3, 4], 0);
        // both peers have slot 1 confirmed
        cs.update_internal(1, vec![epoch_slot1, epoch_slot2]);
        let ci1 = ContactInfo::new(pk1, /*wallclock:*/ 0, /*shred_version:*/ 0);
        let ci2 = ContactInfo::new(pk2, /*wallclock:*/ 0, /*shred_version:*/ 0);

        assert_eq!(
            cs.compute_weights(1, &[ci1, ci2]),
            vec![1000, 10],
            "weights should match the stakes"
        );
    }

    #[test]
    fn test_best_peer_3() {
        agave_logger::setup_with_default("info");
        let cs = ClusterSlots::default();
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let pk_other = Pubkey::new_unique();
        //set stakes of pk1 high and pk2 to unstaked
        let validator_stakes: HashMap<_, _> =
            [(pk1, 42), (pk_other, u64::MAX / 2)].into_iter().collect();
        cs.fake_epoch_info_for_tests(validator_stakes);
        let mut epoch_slot = EpochSlots {
            from: pk_other,
            ..Default::default()
        };
        epoch_slot.fill(&[1, 2, 3, 4], 0);
        // neither pk1 or pk2 has any confirmed slots
        cs.update_internal(0, vec![epoch_slot]);
        let c1 = ContactInfo::new(pk1, /*wallclock:*/ 0, /*shred_version:*/ 0);
        let c2 = ContactInfo::new(pk2, /*wallclock:*/ 0, /*shred_version:*/ 0);
        assert_eq!(
            cs.compute_weights(1, &[c1, c2]),
            vec![42 / 2, 1],
            "weights should be halved, but never zero"
        );
    }

    #[test]
    fn test_best_completed_slot_peer() {
        let cs = ClusterSlots::default();
        let contact_infos: Vec<_> = std::iter::repeat_with(|| {
            ContactInfo::new(
                Pubkey::new_unique(),
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
        let validator_stakes: HashMap<_, _> = [
            (*contact_infos[0].pubkey(), 42),
            (*contact_infos[1].pubkey(), u64::MAX / 2),
        ]
        .into_iter()
        .collect();
        cs.fake_epoch_info_for_tests(validator_stakes);

        // Mark the first validator as completed slot 9, should pick that validator,
        // even though it only has minimal stake, while the other validator has
        // max stake
        cs.insert_node_id(slot, *contact_infos[0].pubkey());
        let (w, i) = cs.compute_weights_exclude_nonfrozen(slot, &contact_infos);
        assert_eq!(w, [42]);
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
        cs.fake_epoch_info_for_tests(map);
        cs.update_internal(0, vec![epoch_slot]);
        assert!(cs.lookup(1).is_some(), "slot 1 should have records");
        assert_eq!(
            cs.lookup(1).unwrap().get_support_by_pubkey(&pk).unwrap(),
            42,
            "the stake of the node should be committed to the slot"
        );
    }
}
