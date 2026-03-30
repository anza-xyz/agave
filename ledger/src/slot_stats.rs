use {
    crate::blockstore_meta::{BlockLocation, SlotMeta},
    bitflags::bitflags,
    lru::LruCache,
    solana_clock::Slot,
    std::{collections::HashMap, sync::Mutex},
};

const SLOTS_STATS_CACHE_CAPACITY: usize = 300;

#[derive(Copy, Clone, Debug)]
pub(crate) enum ShredSource {
    Turbine,
    Repaired,
    Recovered,
}

bitflags! {
    #[derive(Copy, Clone, Default)]
    struct SlotFlags: u8 {
        const DEAD   = 0b00000001;
        const ROOTED = 0b00000010;
    }
}

#[derive(Clone, Default)]
struct LocationShredStats {
    turbine_fec_set_index_counts: HashMap</*fec_set_index*/ u32, /*count*/ usize>,
    num_repaired: usize,
    num_recovered: usize,
    last_index: u64,
    is_full: bool,
}

#[derive(Clone, Default)]
pub struct SlotStats {
    original: LocationShredStats,
    alternate: LocationShredStats,
    flags: SlotFlags,
}

impl SlotStats {
    fn min_index_count(stats: &LocationShredStats) -> usize {
        stats
            .turbine_fec_set_index_counts
            .values()
            .min()
            .copied()
            .unwrap_or_default()
    }

    fn location_stats(&self, location: BlockLocation) -> &LocationShredStats {
        match location {
            BlockLocation::Original => &self.original,
            BlockLocation::Alternate { .. } => &self.alternate,
        }
    }

    fn location_stats_mut(&mut self, location: BlockLocation) -> &mut LocationShredStats {
        match location {
            BlockLocation::Original => &mut self.original,
            BlockLocation::Alternate { .. } => &mut self.alternate,
        }
    }

    pub fn get_min_index_count(&self, location: BlockLocation) -> usize {
        Self::min_index_count(self.location_stats(location))
    }

    fn report_location(&self, slot: Slot, name: &'static str, location_stats: &LocationShredStats) {
        let min_fec_set_count = Self::min_index_count(location_stats);
        datapoint_info!(
            name,
            ("slot", slot, i64),
            ("last_index", location_stats.last_index, i64),
            ("num_repaired", location_stats.num_repaired, i64),
            ("num_recovered", location_stats.num_recovered, i64),
            ("min_turbine_fec_set_count", min_fec_set_count, i64),
            ("is_full", location_stats.is_full, bool),
            ("is_rooted", self.flags.contains(SlotFlags::ROOTED), bool),
            ("is_dead", self.flags.contains(SlotFlags::DEAD), bool),
        );
    }

    fn should_report(location_stats: &LocationShredStats) -> bool {
        location_stats.is_full
            || location_stats.num_recovered > 0
            || location_stats.num_repaired > 0
            || !location_stats.turbine_fec_set_index_counts.is_empty()
    }

    fn report(&self, slot: Slot) {
        self.report_location(slot, "slot_stats_tracking_complete", &self.original);
        if Self::should_report(&self.alternate) {
            self.report_location(
                slot,
                "slot_stats_tracking_complete_alternate",
                &self.alternate,
            );
        }
    }
}

pub struct SlotsStats {
    pub stats: Mutex<LruCache<Slot, SlotStats>>,
}

impl Default for SlotsStats {
    fn default() -> Self {
        Self {
            stats: Mutex::new(LruCache::new(SLOTS_STATS_CACHE_CAPACITY)),
        }
    }
}

impl SlotsStats {
    /// Returns a mutable reference to [`SlotStats`] associated with the slot in the stats LruCache
    /// and a possibly evicted cache entry.
    ///
    /// A new SlotStats entry will be inserted if there is not one present for `slot`; insertion
    /// may cause an existing entry to be evicted.
    fn get_or_default_with_eviction_check(
        stats: &mut LruCache<Slot, SlotStats>,
        slot: Slot,
    ) -> (&mut SlotStats, Option<(Slot, SlotStats)>) {
        let evicted = if stats.contains(&slot) {
            None
        } else {
            // insert slot in cache which might potentially evict an entry
            let evicted = stats.push(slot, SlotStats::default());
            if let Some((evicted_slot, _)) = evicted {
                assert_ne!(evicted_slot, slot);
            }
            evicted
        };
        (stats.get_mut(&slot).unwrap(), evicted)
    }

    pub(crate) fn record_shred(
        &self,
        slot: Slot,
        location: BlockLocation,
        fec_set_index: u32,
        source: ShredSource,
        slot_meta: Option<&SlotMeta>,
    ) {
        let (slot_full_reporting_info, evicted) = {
            let mut stats = self.stats.lock().unwrap();
            let (slot_stats, evicted) = Self::get_or_default_with_eviction_check(&mut stats, slot);
            let location_stats = slot_stats.location_stats_mut(location);
            match source {
                ShredSource::Recovered => location_stats.num_recovered += 1,
                ShredSource::Repaired => location_stats.num_repaired += 1,
                ShredSource::Turbine => {
                    *location_stats
                        .turbine_fec_set_index_counts
                        .entry(fec_set_index)
                        .or_default() += 1
                }
            }
            let mut slot_full_reporting_info = None;
            if let Some(meta) = slot_meta {
                if meta.is_full() {
                    location_stats.last_index = meta.last_index.unwrap();
                    if !location_stats.is_full {
                        location_stats.is_full = true;
                        slot_full_reporting_info = Some((
                            location,
                            location_stats.num_repaired,
                            location_stats.num_recovered,
                        ));
                    }
                }
            }
            (slot_full_reporting_info, evicted)
        };
        if let Some((location, num_repaired, num_recovered)) = slot_full_reporting_info {
            let slot_meta = slot_meta.unwrap();
            let total_time_ms =
                solana_time_utils::timestamp().saturating_sub(slot_meta.first_shred_timestamp);
            let last_index = slot_meta
                .last_index
                .and_then(|ix| i64::try_from(ix).ok())
                .unwrap_or(-1);
            match location {
                BlockLocation::Original => datapoint_info!(
                    "shred_insert_is_full",
                    ("slot", slot, i64),
                    ("total_time_ms", total_time_ms, i64),
                    ("last_index", last_index, i64),
                    ("num_repaired", num_repaired, i64),
                    ("num_recovered", num_recovered, i64),
                ),
                BlockLocation::Alternate { .. } => datapoint_info!(
                    "shred_insert_is_full_alternate",
                    ("slot", slot, i64),
                    ("total_time_ms", total_time_ms, i64),
                    ("last_index", last_index, i64),
                    ("num_repaired", num_repaired, i64),
                    ("num_recovered", num_recovered, i64),
                ),
            }
        }
        if let Some((evicted_slot, evicted_stats)) = evicted {
            evicted_stats.report(evicted_slot);
        }
    }

    fn add_flag(&self, slot: Slot, flag: SlotFlags) {
        let evicted = {
            let mut stats = self.stats.lock().unwrap();
            let (slot_stats, evicted) = Self::get_or_default_with_eviction_check(&mut stats, slot);
            slot_stats.flags |= flag;
            evicted
        };
        if let Some((evicted_slot, evicted_stats)) = evicted {
            evicted_stats.report(evicted_slot);
        }
    }

    /// Marks the slot as dead.
    /// Note this refers to the original column: the alternate column can never be dead
    /// - shreds are pre-verified and we never replay out of the alternate column
    pub fn mark_dead(&self, slot: Slot) {
        self.add_flag(slot, SlotFlags::DEAD);
    }

    /// Marks the original column as rooted.
    /// Note this refers to the original column: the alternate column can never be rooted
    /// - we never replay out of the alternate column and rooting requires replay.
    pub fn mark_rooted(&self, slot: Slot) {
        self.add_flag(slot, SlotFlags::ROOTED);
    }
}
