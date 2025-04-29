//! The admission policy for shreds.
//!
//! It is not a standing configuration: every field is read from the node's state.

pub use solana_shred_wire_format::constants::DATA_SHREDS_PER_FEC_BLOCK;
use {
    crate::error::RejectReason,
    solana_clock::Slot,
    solana_shred_wire_format::headers::{CodeHeader, CommonHeader, DataHeader},
};

/// Bounds a shred's headers must fall within to be worth verifying.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionPolicy {
    /// The only shred version this node accepts, derived from the genesis hash and hard forks.
    pub shred_version: u16,
    /// The node's current root. Shreds at or below it are of no further use.
    pub root: Slot,
    /// Highest slot worth accepting, to bound how far ahead a peer can push us.
    pub max_slot: Slot,
    /// Exclusive upper bound on data shred indices, for the slot this policy was resolved against.
    pub max_data_shreds_per_slot: u32,
    /// Exclusive upper bound on code shred indices, for the slot this policy was resolved against.
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

/// Applies the admission checks specific to data shreds.
///
/// The kind-specific half of [`check_policy`](crate::shred::Shred::check_policy). It lives here
/// rather than on [`ShredLayout`](crate::kind::ShredLayout) because the layout is defined in a
/// crate that knows nothing about this node's view of the cluster.
pub fn admit_data(
    common: &CommonHeader,
    header: &DataHeader,
    policy: &AdmissionPolicy,
) -> Result<(), RejectReason> {
    if !policy.is_data_index_in_bounds(common.index) {
        return Err(RejectReason::IndexOutOfBounds {
            index: common.index,
        });
    }
    let bad_parent = || RejectReason::BadParentOffset {
        slot: common.slot,
        parent_offset: header.parent_offset,
    };
    let parent = common
        .slot
        .checked_sub(Slot::from(header.parent_offset))
        .ok_or_else(bad_parent)?;
    if !policy.are_slots_chainable(common.slot, parent) {
        return Err(bad_parent());
    }
    // Under the fixed erasure configuration, an FEC set is complete exactly at its last index.
    let ends_fec_set = common
        .fec_set_index
        .checked_add(DATA_SHREDS_PER_FEC_BLOCK)
        .and_then(|end| end.checked_sub(1))
        == Some(common.index);
    if header.flags.data_complete() && !ends_fec_set {
        return Err(RejectReason::UnexpectedDataCompleteShred);
    }
    if header.flags.last_in_slot() && !can_end_slot(common.index) {
        return Err(RejectReason::MisalignedLastDataIndex);
    }
    Ok(())
}

/// Applies the admission checks specific to code shreds.
///
/// See [`admit_data`].
pub fn admit_code(
    common: &CommonHeader,
    header: &CodeHeader,
    policy: &AdmissionPolicy,
) -> Result<(), RejectReason> {
    if !policy.is_code_index_in_bounds(common.index) {
        return Err(RejectReason::IndexOutOfBounds {
            index: common.index,
        });
    }
    if common.slot <= policy.root {
        return Err(RejectReason::SlotOutOfRange { slot: common.slot });
    }
    let fixed = u32::from(header.num_data_shreds) == DATA_SHREDS_PER_FEC_BLOCK
        && u32::from(header.num_code_shreds) == DATA_SHREDS_PER_FEC_BLOCK;
    if !fixed {
        return Err(RejectReason::MisalignedErasureConfig {
            num_data_shreds: header.num_data_shreds,
            num_code_shreds: header.num_code_shreds,
        });
    }
    // `position` is what places this shred's leaf in its FEC set's Merkle tree, so a value that
    // does not agree with the shred's own index would prove a leaf of a batch this shred does
    // not belong to. The two counters advance together under the fixed configuration, so the
    // position is the distance from the FEC set index.
    let position = u32::from(header.position);
    let aligned = position < DATA_SHREDS_PER_FEC_BLOCK
        && common.index.checked_sub(common.fec_set_index) == Some(position);
    if !aligned {
        return Err(RejectReason::MisalignedCodePosition {
            index: common.index,
            fec_set_index: common.fec_set_index,
            position: header.position,
        });
    }
    Ok(())
}
