//! The admission policy: everything the [`admit`](crate::Shred::admit) transition needs to know
//! about the node's current view of the cluster.
//!
//! Deliberately plain data. Resolving these values is the caller's job.

use solana_clock::Slot;

/// Number of data shreds, and of code shreds, in every FEC set.
pub const DATA_SHREDS_PER_FEC_BLOCK: u32 = 32;

/// Bounds a shred's headers must fall within to be worth verifying.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionPolicy {
    /// The only shred version this node accepts, `hash(genesis_block) % 65536`.
    pub shred_version: u16,
    /// The node's current root. Shreds at or below it are of no further use.
    pub root: Slot,
    /// Highest slot worth accepting, to bound how far ahead a peer can push us.
    pub max_slot: Slot,
    /// Exclusive upper bound on data shred indices within a slot.
    pub max_data_shreds_per_slot: u32,
    /// Exclusive upper bound on code shred indices within a slot.
    pub max_code_shreds_per_slot: u32,
}

impl AdmissionPolicy {
    /// Whether `index` may address a data shred.
    #[inline]
    pub const fn is_data_index_in_bounds(&self, index: u32) -> bool {
        index < self.max_data_shreds_per_slot
    }

    /// Whether `index` may address a code shred.
    #[inline]
    pub const fn is_code_index_in_bounds(&self, index: u32) -> bool {
        index < self.max_code_shreds_per_slot
    }

    /// Whether a data shred in `slot` may chain to `parent`.
    ///
    /// Slot zero chaining to itself at root zero is the genesis special case; otherwise the parent
    /// must be at or after the root and strictly before the slot.
    #[inline]
    pub const fn are_slots_chainable(&self, slot: Slot, parent: Slot) -> bool {
        if slot == 0 && parent == 0 && self.root == 0 {
            return true;
        }
        self.root <= parent && parent < slot
    }
}

/// Whether `index` is consistent with belonging to the FEC set starting at `fec_set_index`, under
/// the fixed 32:32 erasure configuration.
#[inline]
pub const fn is_fec_set_aligned(index: u32, fec_set_index: u32) -> bool {
    let Some(fec_set_end) = fec_set_index.checked_add(DATA_SHREDS_PER_FEC_BLOCK) else {
        return false;
    };
    index >= fec_set_index
        && index < fec_set_end
        && fec_set_index.is_multiple_of(DATA_SHREDS_PER_FEC_BLOCK)
}

/// Whether `index` can be the last data shred of a slot, which requires it to end an FEC set.
#[inline]
pub const fn can_end_slot(index: u32) -> bool {
    index
        .saturating_add(1)
        .is_multiple_of(DATA_SHREDS_PER_FEC_BLOCK)
}
