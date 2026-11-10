//! Types and helpers for deferring FEC completion during asynchronous SIMD-0504 checks.
//!
//! This module defines a map of held shreds and a lookup that includes them.
//! It also defines a metadata view for insertion checks. These helpers include
//! held data when checking parents, slot boundaries, and slot completeness.
//! They do not add held data to the stored metadata that replay uses.
//!
//! Recovery and FEC validation run in a separate thread from shred insertion.
//! This lets insertion continue receiving shreds while recovery and validation run.
//! Replay reads completed data ranges from blockstore, so insertion must not make
//! a range complete before the required SIMD-0504 checks finish.
//!
//! Insertion therefore holds the last needed data shred to arrive for each FEC set.
//! The missing shred prevents replay from reading that range too early.
//! The insertion mutex owns these payloads across batches.
//! The recovery thread sends its validation result back to the insertion thread.
//! [`Blockstore::insert_shreds_with_recovered`] applies that result and writes the held shred.
//! If the required check failed, it writes the dead-slot flag in the same batch.

use {
    super::{Blockstore, BlockstoreError, ParentInfo, Result},
    crate::{
        blockstore_db::WriteBatch,
        blockstore_meta::{BlockLocation, Index, SlotMeta},
        shred::{DATA_SHREDS_PER_FEC_BLOCK, ErasureSetId, Shred, ShredId, ShredType},
    },
    std::collections::{BTreeMap, HashMap},
};

/// A received data shred held until its FEC check finishes.
/// Keep its repair status so insertion can count it in the correct metric.
pub(super) struct DeferredDataShred {
    pub shred: Shred,
    pub is_repaired: bool,
    /// A task was built for this held shred. This does not mean validation passed.
    /// Repair can clear the flag to retry work that was discarded.
    pub validation_scheduled: bool,
}

/// One received data shred per FEC set, held until insertion applies its check result.
/// These shreds are not in blockstore or its stored shred index.
/// The insertion mutex protects this map across batches.
/// Purges and block switches remove affected entries under the same lock.
/// This prevents insertion from writing held data from a removed block.
/// Keys sort by slot, then FEC index, so cleanup can select a slot range.
pub(super) type DeferredDataShreds = BTreeMap<ErasureSetId, DeferredDataShred>;

/// Look up received shreds without copying the current insertion batch.
#[derive(Clone, Copy)]
pub(super) struct ShredLookup<'a> {
    current: &'a HashMap<(BlockLocation, ShredId), Shred>,
    deferred: Option<&'a DeferredDataShreds>,
}

impl<'a> ShredLookup<'a> {
    pub(super) fn new(
        current: &'a HashMap<(BlockLocation, ShredId), Shred>,
        deferred: Option<&'a DeferredDataShreds>,
    ) -> Self {
        Self { current, deferred }
    }

    pub(super) fn get(&self, key: &(BlockLocation, ShredId)) -> Option<&'a Shred> {
        let (location, id) = *key;
        if let Some(shred) = self.current.get(key) {
            return Some(shred);
        }
        if !matches!(location, BlockLocation::Original) {
            return None;
        }
        let (slot, index, kind) = id.unpack();
        if kind != ShredType::Data {
            return None;
        }
        let width = DATA_SHREDS_PER_FEC_BLOCK as u32;
        self.deferred?
            .get(&ErasureSetId::new(slot, index / width * width))
            .filter(|pending| pending.shred.id() == id)
            .map(|pending| &pending.shred)
    }
}

impl<'a> From<&'a HashMap<(BlockLocation, ShredId), Shred>> for ShredLookup<'a> {
    fn from(current: &'a HashMap<(BlockLocation, ShredId), Shred>) -> Self {
        Self::new(current, None)
    }
}

/// Metadata for checking incoming shreds, including data held for validation.
#[derive(Clone, Copy, Debug)]
pub(super) struct AdmissionMeta {
    pub received: u64,
    pub last_index: Option<u64>,
    pub parent_slot: Option<solana_clock::Slot>,
    pub parent_info: Option<ParentInfo>,
}

impl From<&SlotMeta> for AdmissionMeta {
    fn from(meta: &SlotMeta) -> Self {
        Self {
            received: meta.received,
            last_index: meta.last_index,
            parent_slot: meta.parent_slot,
            parent_info: ParentInfo::from_slot_meta(meta),
        }
    }
}

impl AdmissionMeta {
    pub(super) fn has_update_parent(&self) -> bool {
        self.parent_info
            .is_some_and(|parent| parent.has_update_parent())
    }

    pub(super) fn update_parent(&mut self, parent: ParentInfo) {
        self.parent_slot = Some(parent.parent_block.slot);
        self.parent_info = Some(parent);
    }
}

impl Blockstore {
    /// Build metadata for insertion checks that includes held shreds.
    pub(super) fn build_admission_meta(
        &self,
        meta: &SlotMeta,
        lookup: ShredLookup<'_>,
        write_batch: &mut WriteBatch,
    ) -> Result<AdmissionMeta> {
        let mut admission = AdmissionMeta::from(meta);
        if let Some(pending) = lookup.deferred {
            let range = ErasureSetId::new(meta.slot, 0)..=ErasureSetId::new(meta.slot, u32::MAX);
            for (_, deferred) in pending.range(range) {
                let shred = &deferred.shred;
                record_received(&mut admission, shred);
                if shred
                    .index()
                    .is_multiple_of(DATA_SHREDS_PER_FEC_BLOCK as u32)
                    || shred.data_complete()
                {
                    let parent = shred.parent().map_err(|err| {
                        BlockstoreError::InvalidShredData(format!(
                            "invalid deferred shred parent: {err}"
                        ))
                    })?;
                    if let Some(parent) = self.check_parent_info(
                        shred,
                        parent,
                        BlockLocation::Original,
                        admission.parent_info,
                        self.parse_parent_info(shred, BlockLocation::Original, lookup),
                        write_batch,
                    )? {
                        admission.update_parent(parent);
                    }
                }
            }
        }
        Ok(admission)
    }
}

/// Check whether all slot data has arrived, including held shreds.
/// A conflict must not mark the slot dead if all its data has arrived.
/// This does not allow replay to read held data.
pub(super) fn received_slot_complete(
    meta: &SlotMeta,
    index: &Index,
    pending: &DeferredDataShreds,
) -> bool {
    let range = ErasureSetId::new(meta.slot, 0)..=ErasureSetId::new(meta.slot, u32::MAX);
    let last = meta.last_index.or_else(|| {
        pending.range(range.clone()).find_map(|(_, deferred)| {
            deferred
                .shred
                .last_in_slot()
                .then(|| u64::from(deferred.shred.index()))
        })
    });
    let Some(last) = last else { return false };
    let Some(expected) = last.checked_add(1) else {
        return false;
    };
    let stored = index.data().count_range(0..=last);
    let held = pending
        .range(range)
        .filter(|(_, deferred)| {
            let shred_index = u64::from(deferred.shred.index());
            shred_index <= last && !index.data().contains(shred_index)
        })
        .count();
    (stored + held) as u64 == expected
}

/// Update the highest received index and the last index from a received shred.
/// Do not change which data replay can read.
pub(super) fn record_received(meta: &mut AdmissionMeta, shred: &Shred) {
    meta.received = meta.received.max(u64::from(shred.index()) + 1);
    if shred.last_in_slot() && meta.last_index.is_none() {
        meta.last_index = Some(u64::from(shred.index()));
    }
}
