use {
    crate::{
        bit_vec::BitVec,
        shred::{self, Shred, ShredType, DATA_SHREDS_PER_FEC_BLOCK, MAX_DATA_SHREDS_PER_SLOT},
    },
    bitflags::bitflags,
    serde::{Deserialize, Deserializer, Serialize, Serializer},
    solana_clock::{Slot, UnixTimestamp},
    solana_hash::Hash,
    std::{
        collections::BTreeSet,
        ops::{Range, RangeBounds},
    },
};

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
    /// Flags to indicate whether a slot is a descendant of a slot on the main fork
    pub struct ConnectedFlags:u8 {
        // A slot S should be considered to be connected if:
        // 1) S is a rooted slot itself OR
        // 2) S's parent is connected AND S is full (S's complete block present)
        //
        // 1) is a straightforward case, roots are finalized blocks on the main fork
        // so by definition, they are connected. All roots are connected, but not
        // all connected slots are (or will become) roots.
        //
        // Based on the criteria stated in 2), S is connected iff it has a series
        // of ancestors (that are each connected) that form a chain back to
        // some root slot.
        //
        // A ledger that is updating with a cluster will have either begun at
        // genesis or at some snapshot slot.
        // - Genesis is obviously a special case, and slot 0's parent is deemed
        //   to be connected in order to kick off the induction
        // - Snapshots are taken at rooted slots, and as such, the snapshot slot
        //   should be marked as connected so that a connected chain can start
        //
        // CONNECTED is explicitly the first bit to ensure backwards compatibility
        // with the boolean field that ConnectedFlags replaced in SlotMeta.
        const CONNECTED        = 0b0000_0001;
        const PARENT_CONNECTED = 0b1000_0000;
    }
}

impl Default for ConnectedFlags {
    fn default() -> Self {
        ConnectedFlags::empty()
    }
}

/// Legacy completed data indexes type; de/serialization is inefficient for a BTreeSet.
///
/// Replaced by [`CompletedDataIndexesV2`].
pub type CompletedDataIndexesV1 = BTreeSet<u32>;
/// A fixed size BitVec offers fast lookup and fast de/serialization.
///
/// Supersedes [`CompletedDataIndexesV1`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct CompletedDataIndexesV2 {
    index: BitVec<MAX_DATA_SHREDS_PER_SLOT>,
}

// API for CompletedDataIndexesV2 that mirrors BTreeSet<u32> to make migration easier.
// This allows CompletedDataIndexesV2 to be a drop-in replacement for CompletedDataIndexesV1.
impl CompletedDataIndexesV2 {
    #[inline]
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        self.index.iter_ones().map(|i| i as u32)
    }

    /// Only needed for V1 / V2 test compatibility.
    ///
    /// TODO: Remove once the migration is complete.
    #[cfg(test)]
    #[inline]
    pub fn into_iter(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        self.iter()
    }

    #[inline]
    pub fn insert(&mut self, index: u32) {
        self.index.insert_unchecked(index as usize);
    }

    #[inline]
    pub fn contains(&self, index: &u32) -> bool {
        self.index.contains(*index as usize)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    #[inline]
    pub fn range<R>(&self, bounds: R) -> impl DoubleEndedIterator<Item = u32> + '_
    where
        R: RangeBounds<u32>,
    {
        let start = bounds.start_bound().map(|&b| b as usize);
        let end = bounds.end_bound().map(|&b| b as usize);
        self.index.range((start, end)).iter_ones().map(|i| i as u32)
    }
}

impl FromIterator<u32> for CompletedDataIndexesV2 {
    fn from_iter<T: IntoIterator<Item = u32>>(iter: T) -> Self {
        let index = iter.into_iter().map(|i| i as usize).collect();
        CompletedDataIndexesV2 { index }
    }
}

impl From<CompletedDataIndexesV2> for CompletedDataIndexesV1 {
    fn from(value: CompletedDataIndexesV2) -> Self {
        value.iter().collect()
    }
}

impl From<CompletedDataIndexesV1> for CompletedDataIndexesV2 {
    fn from(value: CompletedDataIndexesV1) -> Self {
        value.into_iter().collect()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
/// The Meta column family
pub struct SlotMetaBase<T> {
    /// The number of slots above the root (the genesis block). The first
    /// slot has slot 0.
    pub slot: Slot,
    /// The total number of consecutive shreds starting from index 0 we have received for this slot.
    /// At the same time, it is also an index of the first missing shred for this slot, while the
    /// slot is incomplete.
    pub consumed: u64,
    /// The index *plus one* of the highest shred received for this slot.  Useful
    /// for checking if the slot has received any shreds yet, and to calculate the
    /// range where there is one or more holes: `(consumed..received)`.
    pub received: u64,
    /// The timestamp of the first time a shred was added for this slot
    pub first_shred_timestamp: u64,
    /// The index of the shred that is flagged as the last shred for this slot.
    /// None until the shred with LAST_SHRED_IN_SLOT flag is received.
    #[serde(with = "serde_compat")]
    pub last_index: Option<u64>,
    /// The slot height of the block this one derives from.
    /// The parent slot of the head of a detached chain of slots is None.
    #[serde(with = "serde_compat")]
    pub parent_slot: Option<Slot>,
    /// The list of slots, each of which contains a block that derives
    /// from this one.
    pub next_slots: Vec<Slot>,
    /// Connected status flags of this slot
    pub connected_flags: ConnectedFlags,
    /// Shreds indices which are marked data complete.  That is, those that have the
    /// [`ShredFlags::DATA_COMPLETE_SHRED`][`crate::shred::ShredFlags::DATA_COMPLETE_SHRED`] set.
    pub completed_data_indexes: T,
}

pub type SlotMetaV1 = SlotMetaBase<CompletedDataIndexesV1>;
pub type SlotMetaV2 = SlotMetaBase<CompletedDataIndexesV2>;

impl From<SlotMetaV1> for SlotMetaV2 {
    fn from(value: SlotMetaV1) -> Self {
        SlotMetaV2 {
            slot: value.slot,
            consumed: value.consumed,
            received: value.received,
            first_shred_timestamp: value.first_shred_timestamp,
            last_index: value.last_index,
            parent_slot: value.parent_slot,
            next_slots: value.next_slots,
            connected_flags: value.connected_flags,
            completed_data_indexes: value.completed_data_indexes.into(),
        }
    }
}

impl From<SlotMetaV2> for SlotMetaV1 {
    fn from(value: SlotMetaV2) -> Self {
        SlotMetaV1 {
            slot: value.slot,
            consumed: value.consumed,
            received: value.received,
            first_shred_timestamp: value.first_shred_timestamp,
            last_index: value.last_index,
            parent_slot: value.parent_slot,
            next_slots: value.next_slots,
            connected_flags: value.connected_flags,
            completed_data_indexes: value.completed_data_indexes.into(),
        }
    }
}

// We need to maintain both formats during migration,
// as both formats will need to be supported when reading
// from rocksdb until the migration is complete.
//
// Swap these types to migrate to the new format.
//
// For example, to enable the new format,
//
// ```
// pub type SlotMeta = SlotMetaV2;
// pub type CompletedDataIndexes = CompletedDataIndexesV2;
// pub type SlotMetaFallback = SlotMetaV1;
// ```
//
// To enable the old format,
//
// ```
// pub type SlotMeta = SlotMetaV1;
// pub type CompletedDataIndexes = CompletedDataIndexesV1;
// pub type SlotMetaFallback = SlotMetaV2;
// ```
pub type SlotMeta = SlotMetaV2;
pub type CompletedDataIndexes = CompletedDataIndexesV2;
pub type SlotMetaFallback = SlotMetaV1;

// Serde implementation of serialize and deserialize for Option<u64>
// where None is represented as u64::MAX; for backward compatibility.
mod serde_compat {
    use super::*;

    pub(super) fn serialize<S>(val: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        val.unwrap_or(u64::MAX).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = u64::deserialize(deserializer)?;
        Ok((val != u64::MAX).then_some(val))
    }
}

pub type Index = IndexV2;
pub type ShredIndex = ShredIndexV2;
/// We currently support falling back to the previous format for migration purposes.
///
/// See https://github.com/anza-xyz/agave/issues/3570.
pub type IndexFallback = IndexV1;
pub type ShredIndexFallback = ShredIndexV1;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
/// Index recording presence/absence of shreds
pub struct IndexV1 {
    pub slot: Slot,
    data: ShredIndexV1,
    coding: ShredIndexV1,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct IndexV2 {
    pub slot: Slot,
    data: ShredIndexV2,
    coding: ShredIndexV2,
}

impl From<IndexV2> for IndexV1 {
    fn from(index: IndexV2) -> Self {
        IndexV1 {
            slot: index.slot,
            data: index.data.into(),
            coding: index.coding.into(),
        }
    }
}

impl From<IndexV1> for IndexV2 {
    fn from(index: IndexV1) -> Self {
        IndexV2 {
            slot: index.slot,
            data: index.data.into(),
            coding: index.coding.into(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ShredIndexV1 {
    /// Map representing presence/absence of shreds
    index: BTreeSet<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
/// Erasure coding information
pub struct ErasureMeta {
    /// Which erasure set in the slot this is
    #[serde(
        serialize_with = "serde_compat_cast::serialize::<_, u64, _>",
        deserialize_with = "serde_compat_cast::deserialize::<_, u64, _>"
    )]
    fec_set_index: u32,
    /// First coding index in the FEC set
    first_coding_index: u64,
    /// Index of the first received coding shred in the FEC set
    first_received_coding_index: u64,
    /// Erasure configuration for this erasure set
    config: ErasureConfig,
}

// Helper module to serde values by type-casting to an intermediate
// type for backward compatibility.
mod serde_compat_cast {
    use super::*;

    // Serializes a value of type T by first type-casting to type R.
    pub(super) fn serialize<S: Serializer, R, T: Copy>(
        &val: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        R: TryFrom<T> + Serialize,
        <R as TryFrom<T>>::Error: std::fmt::Display,
    {
        R::try_from(val)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    // Deserializes a value of type R and type-casts it to type T.
    pub(super) fn deserialize<'de, D, R, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        R: Deserialize<'de>,
        T: TryFrom<R>,
        <T as TryFrom<R>>::Error: std::fmt::Display,
    {
        R::deserialize(deserializer)
            .map(T::try_from)?
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ErasureConfig {
    pub(crate) num_data: usize,
    pub(crate) num_coding: usize,
}

impl ErasureConfig {
    pub(crate) fn is_fixed(&self) -> bool {
        self.num_data == DATA_SHREDS_PER_FEC_BLOCK && self.num_coding == DATA_SHREDS_PER_FEC_BLOCK
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MerkleRootMeta {
    /// The merkle root, `None` for legacy shreds
    merkle_root: Option<Hash>,
    /// The first received shred index
    first_received_shred_index: u32,
    /// The shred type of the first received shred
    first_received_shred_type: ShredType,
}

#[derive(Deserialize, Serialize)]
pub struct DuplicateSlotProof {
    #[serde(with = "shred::serde_bytes_payload")]
    pub shred1: shred::Payload,
    #[serde(with = "shred::serde_bytes_payload")]
    pub shred2: shred::Payload,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq)]
pub enum FrozenHashVersioned {
    Current(FrozenHashStatus),
}

impl FrozenHashVersioned {
    pub fn frozen_hash(&self) -> Hash {
        match self {
            FrozenHashVersioned::Current(frozen_hash_status) => frozen_hash_status.frozen_hash,
        }
    }

    pub fn is_duplicate_confirmed(&self) -> bool {
        match self {
            FrozenHashVersioned::Current(frozen_hash_status) => {
                frozen_hash_status.is_duplicate_confirmed
            }
        }
    }
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq)]
pub struct FrozenHashStatus {
    pub frozen_hash: Hash,
    pub is_duplicate_confirmed: bool,
}

impl Index {
    pub(crate) fn new(slot: Slot) -> Self {
        Self {
            slot,
            data: ShredIndex::default(),
            coding: ShredIndex::default(),
        }
    }

    pub fn data(&self) -> &ShredIndex {
        &self.data
    }
    pub fn coding(&self) -> &ShredIndex {
        &self.coding
    }

    pub(crate) fn data_mut(&mut self) -> &mut ShredIndex {
        &mut self.data
    }
    pub(crate) fn coding_mut(&mut self) -> &mut ShredIndex {
        &mut self.coding
    }
}

#[cfg(test)]
#[allow(unused)]
impl IndexFallback {
    pub(crate) fn new(slot: Slot) -> Self {
        Self {
            slot,
            data: ShredIndexFallback::default(),
            coding: ShredIndexFallback::default(),
        }
    }

    pub fn data(&self) -> &ShredIndexFallback {
        &self.data
    }
    pub fn coding(&self) -> &ShredIndexFallback {
        &self.coding
    }

    pub(crate) fn data_mut(&mut self) -> &mut ShredIndexFallback {
        &mut self.data
    }
    pub(crate) fn coding_mut(&mut self) -> &mut ShredIndexFallback {
        &mut self.coding
    }
}

/// Superseded by [`ShredIndexV2`].
///
/// TODO: Remove this once new [`ShredIndexV2`] is fully rolled out
/// and no longer relies on it for fallback.
#[cfg(test)]
#[allow(unused)]
impl ShredIndexV1 {
    pub fn num_shreds(&self) -> usize {
        self.index.len()
    }

    pub(crate) fn range<R>(&self, bounds: R) -> impl Iterator<Item = &u64>
    where
        R: RangeBounds<u64>,
    {
        self.index.range(bounds)
    }

    pub(crate) fn contains(&self, index: u64) -> bool {
        self.index.contains(&index)
    }

    pub(crate) fn insert(&mut self, index: u64) {
        self.index.insert(index);
    }

    fn remove(&mut self, index: u64) {
        self.index.remove(&index);
    }
}

/// A bitvec (`Vec<u8>`) of shred indices, where each u8 represents 8 shred indices.
///
/// The current implementation of [`ShredIndex`] utilizes a [`BTreeSet`] to store
/// shred indices. While [`BTreeSet`] remains efficient as operations are amortized
/// over time, the overhead of the B-tree structure becomes significant when frequently
/// serialized and deserialized. In particular:
/// - **Tree Traversal**: Serialization requires walking the non-contiguous tree structure.
/// - **Reconstruction**: Deserialization involves rebuilding the tree in bulk,
///   including dynamic memory allocations and re-balancing nodes.
///
/// In contrast, our bit vec implementation provides:
/// - **Contiguous Memory**: All bits are stored in a contiguous array of u64 words,
///   allowing direct indexing and efficient memory access patterns.
/// - **Direct Range Access**: Can load only the specific words that overlap with a
///   requested range, avoiding unnecessary traversal.
/// - **Simplified Serialization**: The contiguous memory layout allows for efficient
///   serialization/deserialization without tree reconstruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShredIndexV2 {
    index: BitVec<MAX_DATA_SHREDS_PER_SLOT>,
    num_shreds: usize,
}

impl ShredIndexV2 {
    pub fn num_shreds(&self) -> usize {
        self.num_shreds
    }

    #[cfg(test)]
    fn remove(&mut self, index: u64) {
        if self.index.remove_unchecked(index as usize) {
            self.num_shreds -= 1;
        }
    }

    #[allow(unused)]
    pub(crate) fn contains(&self, idx: u64) -> bool {
        self.index.contains(idx as usize)
    }

    pub(crate) fn insert(&mut self, idx: u64) {
        if let Ok(true) = self.index.insert(idx as usize) {
            self.num_shreds += 1;
        }
    }

    pub(crate) fn range<R>(&self, bounds: R) -> impl Iterator<Item = u64> + '_
    where
        R: RangeBounds<u64>,
    {
        let start = bounds.start_bound().map(|&b| b as usize);
        let end = bounds.end_bound().map(|&b| b as usize);
        self.index
            .range((start, end))
            .iter_ones()
            .map(|idx| idx as u64)
    }

    fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.range(0..MAX_DATA_SHREDS_PER_SLOT as u64)
    }
}

impl FromIterator<u64> for ShredIndexV2 {
    fn from_iter<T: IntoIterator<Item = u64>>(iter: T) -> Self {
        let mut index = ShredIndexV2::default();
        for idx in iter {
            index.insert(idx);
        }
        index
    }
}

impl FromIterator<u64> for ShredIndexV1 {
    fn from_iter<T: IntoIterator<Item = u64>>(iter: T) -> Self {
        ShredIndexV1 {
            index: iter.into_iter().collect(),
        }
    }
}

impl From<ShredIndexV1> for ShredIndexV2 {
    fn from(value: ShredIndexV1) -> Self {
        value.index.into_iter().collect()
    }
}

impl From<ShredIndexV2> for ShredIndexV1 {
    fn from(value: ShredIndexV2) -> Self {
        ShredIndexV1 {
            index: value.iter().collect(),
        }
    }
}

impl SlotMeta {
    pub fn is_full(&self) -> bool {
        // last_index is None when it has no information about how
        // many shreds will fill this slot.
        // Note: A full slot with zero shreds is not possible.
        // Should never happen
        if self
            .last_index
            .map(|ix| self.consumed > ix + 1)
            .unwrap_or_default()
        {
            datapoint_error!(
                "blockstore_error",
                (
                    "error",
                    format!(
                        "Observed a slot meta with consumed: {} > meta.last_index + 1: {:?}",
                        self.consumed,
                        self.last_index.map(|ix| ix + 1),
                    ),
                    String
                )
            );
        }

        Some(self.consumed) == self.last_index.map(|ix| ix + 1)
    }

    /// Returns a boolean indicating whether this meta's parent slot is known.
    /// This value being true indicates that this meta's slot is the head of a
    /// detached chain of slots.
    pub(crate) fn is_orphan(&self) -> bool {
        self.parent_slot.is_none()
    }

    /// Returns a boolean indicating whether the meta is connected.
    pub fn is_connected(&self) -> bool {
        self.connected_flags.contains(ConnectedFlags::CONNECTED)
    }

    /// Mark the meta as connected.
    pub fn set_connected(&mut self) {
        assert!(self.is_parent_connected());
        self.connected_flags.set(ConnectedFlags::CONNECTED, true);
    }

    /// Returns a boolean indicating whether the meta's parent is connected.
    pub fn is_parent_connected(&self) -> bool {
        self.connected_flags
            .contains(ConnectedFlags::PARENT_CONNECTED)
    }

    /// Mark the meta's parent as connected.
    /// If the meta is also full, the meta is now connected as well. Return a
    /// boolean indicating whether the meta became connected from this call.
    pub fn set_parent_connected(&mut self) -> bool {
        // Already connected so nothing to do, bail early
        if self.is_connected() {
            return false;
        }

        self.connected_flags
            .set(ConnectedFlags::PARENT_CONNECTED, true);

        if self.is_full() {
            self.set_connected();
        }

        self.is_connected()
    }

    /// Dangerous.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn unset_parent(&mut self) {
        self.parent_slot = None;
    }

    pub fn clear_unconfirmed_slot(&mut self) {
        let old = std::mem::replace(self, SlotMeta::new_orphan(self.slot));
        self.next_slots = old.next_slots;
    }

    pub(crate) fn new(slot: Slot, parent_slot: Option<Slot>) -> Self {
        let connected_flags = if slot == 0 {
            // Slot 0 is the start, mark it as having its' parent connected
            // such that slot 0 becoming full will be updated as connected
            ConnectedFlags::PARENT_CONNECTED
        } else {
            ConnectedFlags::default()
        };
        SlotMeta {
            slot,
            parent_slot,
            connected_flags,
            ..SlotMeta::default()
        }
    }

    pub(crate) fn new_orphan(slot: Slot) -> Self {
        Self::new(slot, /*parent_slot:*/ None)
    }
}

impl ErasureMeta {
    pub(crate) fn from_coding_shred(shred: &Shred) -> Option<Self> {
        match shred.shred_type() {
            ShredType::Data => None,
            ShredType::Code => {
                let config = ErasureConfig {
                    num_data: usize::from(shred.num_data_shreds().ok()?),
                    num_coding: usize::from(shred.num_coding_shreds().ok()?),
                };
                let first_coding_index = u64::from(shred.first_coding_index()?);
                let first_received_coding_index = u64::from(shred.index());
                let erasure_meta = ErasureMeta {
                    fec_set_index: shred.fec_set_index(),
                    config,
                    first_coding_index,
                    first_received_coding_index,
                };
                Some(erasure_meta)
            }
        }
    }

    // Returns true if the erasure fields on the shred
    // are consistent with the erasure-meta.
    pub(crate) fn check_coding_shred(&self, shred: &Shred) -> bool {
        let Some(mut other) = Self::from_coding_shred(shred) else {
            return false;
        };
        other.first_received_coding_index = self.first_received_coding_index;
        self == &other
    }

    /// Returns true if both shreds are coding shreds and have a
    /// consistent erasure config
    pub fn check_erasure_consistency(shred1: &Shred, shred2: &Shred) -> bool {
        let Some(coding_shred) = Self::from_coding_shred(shred1) else {
            return false;
        };
        coding_shred.check_coding_shred(shred2)
    }

    pub(crate) fn config(&self) -> ErasureConfig {
        self.config
    }

    pub(crate) fn data_shreds_indices(&self) -> Range<u64> {
        let num_data = self.config.num_data as u64;
        let fec_set_index = u64::from(self.fec_set_index);
        fec_set_index..fec_set_index + num_data
    }

    pub(crate) fn coding_shreds_indices(&self) -> Range<u64> {
        let num_coding = self.config.num_coding as u64;
        self.first_coding_index..self.first_coding_index + num_coding
    }

    pub(crate) fn first_received_coding_shred_index(&self) -> Option<u32> {
        u32::try_from(self.first_received_coding_index).ok()
    }

    pub(crate) fn next_fec_set_index(&self) -> Option<u32> {
        let num_data = u32::try_from(self.config.num_data).ok()?;
        self.fec_set_index.checked_add(num_data)
    }

    // Returns true if some data shreds are missing, but there are enough data
    // and coding shreds to recover the erasure batch.
    // TODO: In order to retransmit all shreds from the erasure batch, we need
    // to always recover the batch as soon as possible, even if no data shreds
    // are missing. But because we currently do not store recovered coding
    // shreds into the blockstore we cannot identify if the batch was already
    // recovered (and retransmitted) or not.
    pub(crate) fn should_recover_shreds(&self, index: &Index) -> bool {
        let num_data = index.data().range(self.data_shreds_indices()).count();
        if num_data >= self.config.num_data {
            return false; // No data shreds is missing.
        }
        let num_coding = index.coding().range(self.coding_shreds_indices()).count();
        self.config.num_data <= num_data + num_coding
    }

    #[cfg(test)]
    pub(crate) fn clear_first_received_coding_shred_index(&mut self) {
        self.first_received_coding_index = 0;
    }
}

impl MerkleRootMeta {
    pub(crate) fn from_shred(shred: &Shred) -> Self {
        Self {
            // An error here after the shred has already sigverified
            // can only indicate that the leader is sending
            // legacy or malformed shreds. We should still store
            // `None` for those cases in blockstore, as a later
            // shred that contains a proper merkle root would constitute
            // a valid duplicate shred proof.
            merkle_root: shred.merkle_root().ok(),
            first_received_shred_index: shred.index(),
            first_received_shred_type: shred.shred_type(),
        }
    }

    pub(crate) fn merkle_root(&self) -> Option<Hash> {
        self.merkle_root
    }

    pub(crate) fn first_received_shred_index(&self) -> u32 {
        self.first_received_shred_index
    }

    pub(crate) fn first_received_shred_type(&self) -> ShredType {
        self.first_received_shred_type
    }
}

impl DuplicateSlotProof {
    pub(crate) fn new<S, T>(shred1: S, shred2: T) -> Self
    where
        shred::Payload: From<S> + From<T>,
    {
        DuplicateSlotProof {
            shred1: shred::Payload::from(shred1),
            shred2: shred::Payload::from(shred2),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransactionStatusIndexMeta {
    pub max_slot: Slot,
    pub frozen: bool,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AddressSignatureMeta {
    pub writeable: bool,
}

/// Performance information about validator execution during a time slice.
///
/// Older versions should only arise as a result of deserialization of entries stored by a previous
/// version of the validator.  Current version should only produce [`PerfSampleV2`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PerfSample {
    V1(PerfSampleV1),
    V2(PerfSampleV2),
}

impl From<PerfSampleV1> for PerfSample {
    fn from(value: PerfSampleV1) -> PerfSample {
        PerfSample::V1(value)
    }
}

impl From<PerfSampleV2> for PerfSample {
    fn from(value: PerfSampleV2) -> PerfSample {
        PerfSample::V2(value)
    }
}

/// Version of [`PerfSample`] used before 1.15.x.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct PerfSampleV1 {
    pub num_transactions: u64,
    pub num_slots: u64,
    pub sample_period_secs: u16,
}

/// Version of the [`PerfSample`] introduced in 1.15.x.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct PerfSampleV2 {
    // `PerfSampleV1` part
    pub num_transactions: u64,
    pub num_slots: u64,
    pub sample_period_secs: u16,

    // New fields.
    pub num_non_vote_transactions: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct OptimisticSlotMetaV0 {
    pub hash: Hash,
    pub timestamp: UnixTimestamp,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq)]
pub enum OptimisticSlotMetaVersioned {
    V0(OptimisticSlotMetaV0),
}

impl OptimisticSlotMetaVersioned {
    pub fn new(hash: Hash, timestamp: UnixTimestamp) -> Self {
        OptimisticSlotMetaVersioned::V0(OptimisticSlotMetaV0 { hash, timestamp })
    }

    pub fn hash(&self) -> Hash {
        match self {
            OptimisticSlotMetaVersioned::V0(meta) => meta.hash,
        }
    }

    pub fn timestamp(&self) -> UnixTimestamp {
        match self {
            OptimisticSlotMetaVersioned::V0(meta) => meta.timestamp,
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        bincode::Options,
        proptest::prelude::*,
        rand::{prelude::IndexedRandom as _, rng},
    };

    #[test]
    fn test_slot_meta_slot_zero_connected() {
        let meta = SlotMeta::new(0 /* slot */, None /* parent */);
        assert!(meta.is_parent_connected());
        assert!(!meta.is_connected());
    }

    #[test]
    fn test_should_recover_shreds() {
        let fec_set_index = 0;
        let erasure_config = ErasureConfig {
            num_data: 8,
            num_coding: 16,
        };
        let e_meta = ErasureMeta {
            fec_set_index,
            first_coding_index: u64::from(fec_set_index),
            config: erasure_config,
            first_received_coding_index: 0,
        };
        let mut rng = rng();
        let mut index = Index::new(0);

        let data_indexes = 0..erasure_config.num_data as u64;
        let coding_indexes = 0..erasure_config.num_coding as u64;

        assert!(!e_meta.should_recover_shreds(&index));

        for ix in data_indexes.clone() {
            index.data_mut().insert(ix);
        }

        assert!(!e_meta.should_recover_shreds(&index));

        for ix in coding_indexes.clone() {
            index.coding_mut().insert(ix);
        }

        for &idx in data_indexes
            .clone()
            .collect::<Vec<_>>()
            .choose_multiple(&mut rng, erasure_config.num_data)
        {
            index.data_mut().remove(idx);

            assert!(e_meta.should_recover_shreds(&index));
        }

        for ix in data_indexes {
            index.data_mut().insert(ix);
        }

        for &idx in coding_indexes
            .collect::<Vec<_>>()
            .choose_multiple(&mut rng, erasure_config.num_coding)
        {
            index.coding_mut().remove(idx);

            assert!(!e_meta.should_recover_shreds(&index));
        }
    }

    /// Generate a random Range<u64>.
    fn rand_range(range: Range<u64>) -> impl Strategy<Value = Range<u64>> {
        (range.clone(), range).prop_map(
            // Avoid descending (empty) ranges
            |(start, end)| {
                if start > end {
                    end..start
                } else {
                    start..end
                }
            },
        )
    }

    proptest! {
        #[test]
        fn shred_index_legacy_compat(
            shreds in rand_range(0..MAX_DATA_SHREDS_PER_SLOT as u64),
            range in rand_range(0..MAX_DATA_SHREDS_PER_SLOT as u64)
        ) {
            let mut legacy = ShredIndexV1::default();
            let mut v2 = ShredIndexV2::default();

            for i in shreds {
                v2.insert(i);
                legacy.insert(i);
            }

            for &i in legacy.index.iter() {
                assert!(v2.contains(i));
            }

            assert_eq!(v2.num_shreds(), legacy.num_shreds());

            assert_eq!(
                v2.range(range.clone()).sum::<u64>(),
                legacy.range(range).sum::<u64>()
            );

            assert_eq!(ShredIndexV2::from(legacy.clone()), v2.clone());
            assert_eq!(ShredIndexV1::from(v2), legacy);
        }

        /// Property: [`Index`] cannot be deserialized from [`IndexV2`].
        ///
        /// # Failure cases
        /// 1. Empty [`IndexV2`]
        ///     - [`ShredIndex`] deserialization should fail due to trailing bytes of `num_shreds`.
        /// 2. Non-empty [`IndexV2`]
        ///     - Encoded length of [`ShredIndexV2::index`] (`Vec<u8>`) will be relative to a sequence of `u8`,
        ///       resulting in not enough bytes when deserialized into sequence of `u64`.
        #[test]
        fn test_legacy_collision(
            coding_indices in rand_range(0..MAX_DATA_SHREDS_PER_SLOT as u64),
            data_indices in rand_range(0..MAX_DATA_SHREDS_PER_SLOT as u64),
            slot in 0..u64::MAX
        ) {
            let index = IndexV2 {
                coding: coding_indices.into_iter().collect(),
                data: data_indices.into_iter().collect(),
                slot,
            };
            let config = bincode::DefaultOptions::new().with_fixint_encoding().reject_trailing_bytes();
            let legacy = config.deserialize::<IndexV1>(&config.serialize(&index).unwrap());
            prop_assert!(legacy.is_err());
        }

        /// Property: [`IndexV2`] cannot be deserialized from [`Index`].
        ///
        /// # Failure cases
        /// 1. Empty [`Index`]
        ///     - [`ShredIndexV2`] deserialization should fail due to missing `num_shreds` (not enough bytes).
        /// 2. Non-empty [`Index`]
        ///     - Encoded length of [`ShredIndex::index`] (`BTreeSet<u64>`) will be relative to a sequence of `u64`,
        ///       resulting in trailing bytes when deserialized into sequence of `u8`.
        #[test]
        fn test_legacy_collision_inverse(
            coding_indices in rand_range(0..MAX_DATA_SHREDS_PER_SLOT as u64),
            data_indices in rand_range(0..MAX_DATA_SHREDS_PER_SLOT as u64),
            slot in 0..u64::MAX
        ) {
            let index = IndexV1 {
                coding: coding_indices.into_iter().collect(),
                data: data_indices.into_iter().collect(),
                slot,
            };
            let config = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .reject_trailing_bytes();
            let v2 = config.deserialize::<IndexV2>(&config.serialize(&index).unwrap());
            prop_assert!(v2.is_err());
        }

        // Property: range queries should return correct indices
        #[test]
        fn range_query_correctness(
            indices in rand_range(0..MAX_DATA_SHREDS_PER_SLOT as u64),
        ) {
            let mut index = ShredIndexV2::default();

            for idx in indices.clone() {
                index.insert(idx);
            }

            assert_eq!(
                index.range(indices.clone()).collect::<Vec<_>>(),
                indices.into_iter().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn test_shred_index_v2_range_bounds() {
        let mut index = ShredIndexV2::default();

        index.insert(10);
        index.insert(20);
        index.insert(30);
        index.insert(40);

        use std::ops::Bound::*;

        // Test all combinations of bounds
        let test_cases = [
            // (start_bound, end_bound, expected_result)
            (Included(10), Included(30), vec![10, 20, 30]),
            (Included(10), Excluded(30), vec![10, 20]),
            (Excluded(10), Included(30), vec![20, 30]),
            (Excluded(10), Excluded(30), vec![20]),
            // Unbounded start
            (Unbounded, Included(20), vec![10, 20]),
            (Unbounded, Excluded(20), vec![10]),
            // Unbounded end
            (Included(30), Unbounded, vec![30, 40]),
            (Excluded(30), Unbounded, vec![40]),
            // Both Unbounded
            (Unbounded, Unbounded, vec![10, 20, 30, 40]),
        ];

        for (start_bound, end_bound, expected) in test_cases {
            let result: Vec<_> = index.range((start_bound, end_bound)).collect();
            assert_eq!(
                result, expected,
                "Failed for bounds: start={start_bound:?}, end={end_bound:?}"
            );
        }
    }

    #[test]
    fn test_shred_index_v2_boundary_conditions() {
        let mut index = ShredIndexV2::default();

        // First possible index
        index.insert(0);
        // Last index in first word (bits 0-7)
        index.insert(7);
        // First index in second word (bits 8-15)
        index.insert(8);
        // Last index in second word
        index.insert(15);
        // Last valid index
        index.insert(MAX_DATA_SHREDS_PER_SLOT as u64 - 1);
        // Should be ignored (too large)
        index.insert(MAX_DATA_SHREDS_PER_SLOT as u64);

        // Verify contents
        assert!(index.contains(0));
        assert!(index.contains(7));
        assert!(index.contains(8));
        assert!(index.contains(15));
        assert!(index.contains(MAX_DATA_SHREDS_PER_SLOT as u64 - 1));
        assert!(!index.contains(MAX_DATA_SHREDS_PER_SLOT as u64));

        // Cross-word boundary
        assert_eq!(index.range(6..10).collect::<Vec<_>>(), vec![7, 8]);
        // Full first word
        assert_eq!(index.range(0..8).collect::<Vec<_>>(), vec![0, 7]);
        // Full second word
        assert_eq!(index.range(8..16).collect::<Vec<_>>(), vec![8, 15]);

        // Empty ranges
        assert_eq!(index.range(0..0).count(), 0);
        assert_eq!(index.range(1..1).count(), 0);

        // Test range that exceeds max
        let oversized_range = index.range(0..MAX_DATA_SHREDS_PER_SLOT as u64 + 1);
        assert_eq!(oversized_range.count(), 5);
        assert_eq!(index.num_shreds(), 5);

        index.remove(0);
        assert!(!index.contains(0));
        index.remove(7);
        assert!(!index.contains(7));
        index.remove(8);
        assert!(!index.contains(8));
        index.remove(15);
        assert!(!index.contains(15));
        index.remove(MAX_DATA_SHREDS_PER_SLOT as u64 - 1);
        assert!(!index.contains(MAX_DATA_SHREDS_PER_SLOT as u64 - 1));

        assert_eq!(index.num_shreds(), 0);
    }

    #[test]
    fn test_connected_flags_compatibility() {
        // Define a couple structs with bool and ConnectedFlags to illustrate
        // that that ConnectedFlags can be deserialized into a bool if the
        // PARENT_CONNECTED bit is NOT set
        #[derive(Debug, Deserialize, PartialEq, Serialize)]
        struct WithBool {
            slot: Slot,
            connected: bool,
        }
        #[derive(Debug, Deserialize, PartialEq, Serialize)]
        struct WithFlags {
            slot: Slot,
            connected: ConnectedFlags,
        }

        let slot = 3;
        let mut with_bool = WithBool {
            slot,
            connected: false,
        };
        let mut with_flags = WithFlags {
            slot,
            connected: ConnectedFlags::default(),
        };

        // Confirm that serialized byte arrays are same length
        assert_eq!(
            bincode::serialized_size(&with_bool).unwrap(),
            bincode::serialized_size(&with_flags).unwrap()
        );

        // Confirm that connected=false equivalent to ConnectedFlags::default()
        assert_eq!(
            bincode::serialize(&with_bool).unwrap(),
            bincode::serialize(&with_flags).unwrap()
        );

        // Set connected in WithBool and confirm inequality
        with_bool.connected = true;
        assert_ne!(
            bincode::serialize(&with_bool).unwrap(),
            bincode::serialize(&with_flags).unwrap()
        );

        // Set connected in WithFlags and confirm equality regained
        with_flags.connected.set(ConnectedFlags::CONNECTED, true);
        assert_eq!(
            bincode::serialize(&with_bool).unwrap(),
            bincode::serialize(&with_flags).unwrap()
        );

        // Deserializing WithBool into WithFlags succeeds
        assert_eq!(
            with_flags,
            bincode::deserialize::<WithFlags>(&bincode::serialize(&with_bool).unwrap()).unwrap()
        );

        // Deserializing WithFlags into WithBool succeeds
        assert_eq!(
            with_bool,
            bincode::deserialize::<WithBool>(&bincode::serialize(&with_flags).unwrap()).unwrap()
        );

        // Deserializing WithFlags with extra bit set into WithBool fails
        with_flags
            .connected
            .set(ConnectedFlags::PARENT_CONNECTED, true);
        assert!(
            bincode::deserialize::<WithBool>(&bincode::serialize(&with_flags).unwrap()).is_err()
        );
    }

    #[test]
    fn test_clear_unconfirmed_slot() {
        let mut slot_meta = SlotMeta::new_orphan(5);
        slot_meta.consumed = 5;
        slot_meta.received = 5;
        slot_meta.next_slots = vec![6, 7];
        slot_meta.clear_unconfirmed_slot();

        let mut expected = SlotMeta::new_orphan(5);
        expected.next_slots = vec![6, 7];
        assert_eq!(slot_meta, expected);
    }

    // `PerfSampleV2` should contain `PerfSampleV1` as a prefix, in order for the column to be
    // backward and forward compatible.
    #[test]
    fn perf_sample_v1_is_prefix_of_perf_sample_v2() {
        let v2 = PerfSampleV2 {
            num_transactions: 4190143848,
            num_slots: 3607325588,
            sample_period_secs: 31263,
            num_non_vote_transactions: 4056116066,
        };

        let v2_bytes = bincode::serialize(&v2).expect("`PerfSampleV2` can be serialized");

        let actual: PerfSampleV1 = bincode::deserialize(&v2_bytes)
            .expect("Bytes encoded as `PerfSampleV2` can be decoded as `PerfSampleV1`");
        let expected = PerfSampleV1 {
            num_transactions: v2.num_transactions,
            num_slots: v2.num_slots,
            sample_period_secs: v2.sample_period_secs,
        };

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_erasure_meta_transition() {
        #[derive(Debug, Deserialize, PartialEq, Serialize)]
        struct OldErasureMeta {
            set_index: u64,
            first_coding_index: u64,
            #[serde(rename = "size")]
            __unused_size: usize,
            config: ErasureConfig,
        }

        let set_index = 64;
        let erasure_config = ErasureConfig {
            num_data: 8,
            num_coding: 16,
        };
        let mut old_erasure_meta = OldErasureMeta {
            set_index,
            first_coding_index: set_index,
            __unused_size: 0,
            config: erasure_config,
        };
        let mut new_erasure_meta = ErasureMeta {
            fec_set_index: u32::try_from(set_index).unwrap(),
            first_coding_index: set_index,
            first_received_coding_index: 0,
            config: erasure_config,
        };

        assert_eq!(
            bincode::serialized_size(&old_erasure_meta).unwrap(),
            bincode::serialized_size(&new_erasure_meta).unwrap(),
        );

        assert_eq!(
            bincode::deserialize::<ErasureMeta>(&bincode::serialize(&old_erasure_meta).unwrap())
                .unwrap(),
            new_erasure_meta
        );

        new_erasure_meta.first_received_coding_index = u64::from(u32::MAX);
        old_erasure_meta.__unused_size = usize::try_from(u32::MAX).unwrap();

        assert_eq!(
            bincode::deserialize::<OldErasureMeta>(&bincode::serialize(&new_erasure_meta).unwrap())
                .unwrap(),
            old_erasure_meta
        );
    }
}
