//! Traits that define how data is encoded in the RocksDB-backed Blockstore.
use {
    crate::{
        blockstore::error::Result,
        blockstore_meta::{self},
    },
    bincode::Options as BincodeOptions,
    serde::{de::DeserializeOwned, Serialize},
    solana_sdk::{
        clock::{Slot, UnixTimestamp},
        pubkey::{Pubkey, PUBKEY_BYTES},
        signature::{Signature, SIGNATURE_BYTES},
    },
    solana_storage_proto::convert::generated,
};

// To add a new column, declare the type below and implement the applicable
// traits for it. At the very least, Column and ColumnName will be necessary.
//
// Afterwards, update the Rocks implementation to create / load the new column.
// Lastly, remember to account for the column's cleanup so that the column does
// not grow unbounded.

pub mod columns {
    // This avoids relatively obvious `super::` qualifications required for all non-trivial type
    // references in the column doc-comments.
    #[cfg(doc)]
    use super::{blockstore_meta, generated, Pubkey, Signature, Slot, SlotColumn, UnixTimestamp};

    #[derive(Debug)]
    /// The slot metadata column.
    ///
    /// This column family tracks the status of the received shred data for a
    /// given slot.  Tracking the progress as the slot fills up allows us to
    /// know if the slot (or pieces of the slot) are ready to be replayed.
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: [`blockstore_meta::SlotMeta`]
    pub struct SlotMeta;

    #[derive(Debug)]
    /// The orphans column.
    ///
    /// This column family tracks whether a slot has a parent.  Slots without a
    /// parent are by definition orphan slots.  Orphans will have an entry in
    /// this column family with true value.  Once an orphan slot has a parent,
    /// its entry in this column will be deleted.
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: `bool`
    pub struct Orphans;

    #[derive(Debug)]
    /// The dead slots column.
    /// This column family tracks whether a slot is dead.
    ///
    /// A slot is marked as dead if the validator thinks it will never be able
    /// to successfully replay this slot.  Example scenarios include errors
    /// during the replay of a slot, or the validator believes it will never
    /// receive all the shreds of a slot.
    ///
    /// If a slot has been mistakenly marked as dead, the ledger-tool's
    /// --remove-dead-slot can unmark a dead slot.
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: `bool`
    pub struct DeadSlots;

    #[derive(Debug)]
    /// The duplicate slots column
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: [`blockstore_meta::DuplicateSlotProof`]
    pub struct DuplicateSlots;

    #[derive(Debug)]
    /// The erasure meta column.
    ///
    /// This column family stores ErasureMeta which includes metadata about
    /// dropped network packets (or erasures) that can be used to recover
    /// missing data shreds.
    ///
    /// Its index type is `crate::shred::ErasureSetId`, which consists of a Slot ID
    /// and a FEC (Forward Error Correction) set index.
    ///
    /// * index type: `crate::shred::ErasureSetId` `(Slot, fec_set_index: u64)`
    /// * value type: [`blockstore_meta::ErasureMeta`]
    pub struct ErasureMeta;

    #[derive(Debug)]
    /// The bank hash column.
    ///
    /// This column family persists the bank hash of a given slot.  Note that
    /// not every slot has a bank hash (e.g., a dead slot.)
    ///
    /// The bank hash of a slot is derived from hashing the delta state of all
    /// the accounts in a slot combined with the bank hash of its parent slot.
    /// A bank hash of a slot essentially represents all the account states at
    /// that slot.
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: [`blockstore_meta::FrozenHashVersioned`]
    pub struct BankHash;

    #[derive(Debug)]
    /// The root column.
    ///
    /// This column family persists whether a slot is a root.  Slots on the
    /// main fork will be inserted into this column when they are finalized.
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: `bool`
    pub struct Root;

    #[derive(Debug)]
    /// The index column
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: [`blockstore_meta::Index`]
    pub struct Index;

    #[derive(Debug)]
    /// The shred data column
    ///
    /// * index type: `(u64, u64)`
    /// * value type: [`Vec<u8>`]
    pub struct ShredData;

    #[derive(Debug)]
    /// The shred erasure code column
    ///
    /// * index type: `(u64, u64)`
    /// * value type: [`Vec<u8>`]
    pub struct ShredCode;

    #[derive(Debug)]
    /// The transaction status column
    ///
    /// * index type: `(`[`Signature`]`, `[`Slot`])`
    /// * value type: [`generated::TransactionStatusMeta`]
    pub struct TransactionStatus;

    #[derive(Debug)]
    /// The address signatures column
    ///
    /// * index type: `(`[`Pubkey`]`, `[`Slot`]`, u32, `[`Signature`]`)`
    /// * value type: [`blockstore_meta::AddressSignatureMeta`]
    pub struct AddressSignatures;

    #[derive(Debug)]
    /// The transaction memos column
    ///
    /// * index type: [`Signature`]
    /// * value type: [`String`]
    pub struct TransactionMemos;

    #[derive(Debug)]
    /// The transaction status index column.
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: [`blockstore_meta::TransactionStatusIndexMeta`]
    pub struct TransactionStatusIndex;

    #[derive(Debug)]
    /// The rewards column
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: [`generated::Rewards`]
    pub struct Rewards;

    #[derive(Debug)]
    /// The blocktime column
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: [`UnixTimestamp`]
    pub struct Blocktime;

    #[derive(Debug)]
    /// The performance samples column
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: [`blockstore_meta::PerfSample`]
    pub struct PerfSamples;

    #[derive(Debug)]
    /// The block height column
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: `u64`
    pub struct BlockHeight;

    #[derive(Debug)]
    /// The program costs column
    ///
    /// * index type: [`Pubkey`]
    /// * value type: [`blockstore_meta::ProgramCost`]
    pub struct ProgramCosts;

    #[derive(Debug)]
    /// The optimistic slot column
    ///
    /// * index type: `u64` (see [`SlotColumn`])
    /// * value type: [`blockstore_meta::OptimisticSlotMetaVersioned`]
    pub struct OptimisticSlots;

    #[derive(Debug)]
    /// The merkle root meta column
    ///
    /// Each merkle shred is part of a merkle tree for
    /// its FEC set. This column stores that merkle root and associated
    /// meta information about the first shred received.
    ///
    /// Its index type is (Slot, fec_set_index).
    ///
    /// * index type: `crate::shred::ErasureSetId` `(Slot, fec_set_index: u32)`
    /// * value type: [`blockstore_meta::MerkleRootMeta`]`
    pub struct MerkleRootMeta;
}

macro_rules! convert_column_index_to_key_bytes {
    ($key:ident, $($range:expr => $bytes:expr),* $(,)?) => {{
        let mut key = [0u8; std::mem::size_of::<Self::$key>()];
        debug_assert_eq!(0 $(+$bytes.len())*, key.len());
        $(key[$range].copy_from_slice($bytes);)*
        key
    }};
}

macro_rules! convert_column_key_bytes_to_index {
    ($k:ident, $($a:literal..$b:literal => $f:expr),* $(,)?) => {{
        ($($f(<[u8; $b-$a]>::try_from(&$k[$a..$b]).unwrap())),*)
    }};
}

pub trait Column {
    // The logical key for how data will be accessed in this column
    type Index;
    // Byte array representation of the Index type; this is the format RocksDB accepts
    type Key: AsRef<[u8]>;
    // Converts Self::Index to Self::Key
    fn key(index: &Self::Index) -> Self::Key;
    // Converts Self::Key to Self::Index
    fn index(key: &[u8]) -> Self::Index;
    // This trait method is primarily used by `Database::delete_range_cf()`, and is therefore only
    // relevant for columns keyed by Slot: ie. SlotColumns and columns that feature a Slot as the
    // first item in the key.
    fn as_index(slot: Slot) -> Self::Index;
    fn slot(index: Self::Index) -> Slot;
}

// RocksDB has a notion of columns families to group related data. The columns are refer
pub trait ColumnName {
    const NAME: &'static str;
}

// Columns that serialize data on insertion and deserialize on fetch
pub trait TypedColumn: Column {
    type Type: Serialize + DeserializeOwned;

    fn deserialize(data: &[u8]) -> Result<Self::Type> {
        Ok(bincode::deserialize(data)?)
    }

    fn serialize(data: &Self::Type) -> Result<Vec<u8>> {
        Ok(bincode::serialize(data)?)
    }
}

pub trait ProtobufColumn: Column {
    type Type: prost::Message + Default;
}

/// SlotColumn is a trait for slot-based column families.  Its index is
/// essentially Slot (or more generally speaking, has a 1:1 mapping to Slot).
///
/// The clean-up of any LedgerColumn that implements SlotColumn is managed by
/// `LedgerCleanupService`, which will periodically deprecate and purge
/// oldest entries that are older than the latest root in order to maintain the
/// configured --limit-ledger-size under the validator argument.
pub trait SlotColumn<Index = Slot> {}

pub enum IndexError {
    UnpackError,
}

/// Helper trait to transition primary indexes out from the columns that are using them.
pub trait ColumnIndexDeprecation: Column {
    const CURRENT_INDEX_LEN: usize;
    type DeprecatedIndex;
    type DeprecatedKey: AsRef<[u8]>;

    fn deprecated_key(index: Self::DeprecatedIndex) -> Self::DeprecatedKey;
    fn try_deprecated_index(key: &[u8]) -> std::result::Result<Self::DeprecatedIndex, IndexError>;

    fn try_current_index(key: &[u8]) -> std::result::Result<Self::Index, IndexError>;
    fn convert_index(deprecated_index: Self::DeprecatedIndex) -> Self::Index;

    fn index(key: &[u8]) -> Self::Index {
        if let Ok(index) = Self::try_current_index(key) {
            index
        } else if let Ok(index) = Self::try_deprecated_index(key) {
            Self::convert_index(index)
        } else {
            // Way back in the day, we broke the TransactionStatus column key. This fallback
            // preserves the existing logic for ancient keys, but realistically should never be
            // executed.
            Self::as_index(0)
        }
    }
}

impl TypedColumn for columns::AddressSignatures {
    type Type = blockstore_meta::AddressSignatureMeta;
}

impl TypedColumn for columns::TransactionMemos {
    type Type = String;
}

impl TypedColumn for columns::TransactionStatusIndex {
    type Type = blockstore_meta::TransactionStatusIndexMeta;
}

impl<T: SlotColumn> Column for T {
    type Index = Slot;
    type Key = [u8; std::mem::size_of::<Slot>()];

    #[inline]
    fn key(slot: &Self::Index) -> Self::Key {
        slot.to_be_bytes()
    }

    /// Converts a RocksDB key to its u64 Index.
    fn index(key: &[u8]) -> Self::Index {
        convert_column_key_bytes_to_index!(key, 0..8 => Slot::from_be_bytes)
    }

    fn slot(index: Self::Index) -> Slot {
        index
    }

    /// Converts a Slot to its u64 Index.
    fn as_index(slot: Slot) -> u64 {
        slot
    }
}

impl Column for columns::TransactionStatus {
    type Index = (Signature, Slot);
    type Key = [u8; SIGNATURE_BYTES + std::mem::size_of::<Slot>()];

    #[inline]
    fn key((signature, slot): &Self::Index) -> Self::Key {
        convert_column_index_to_key_bytes!(Key,
            ..64 => signature.as_ref(),
            64.. => &slot.to_be_bytes(),
        )
    }

    fn index(key: &[u8]) -> (Signature, Slot) {
        <columns::TransactionStatus as ColumnIndexDeprecation>::index(key)
    }

    fn slot(index: Self::Index) -> Slot {
        index.1
    }

    // The TransactionStatus column is not keyed by slot so this method is meaningless
    // See Column::as_index() declaration for more details
    fn as_index(_index: u64) -> Self::Index {
        (Signature::default(), 0)
    }
}
impl ColumnName for columns::TransactionStatus {
    const NAME: &'static str = "transaction_status";
}
impl ProtobufColumn for columns::TransactionStatus {
    type Type = generated::TransactionStatusMeta;
}

impl ColumnIndexDeprecation for columns::TransactionStatus {
    const CURRENT_INDEX_LEN: usize = 72;
    type DeprecatedIndex = (u64, Signature, Slot);
    type DeprecatedKey = [u8; 80];

    fn deprecated_key((index, signature, slot): Self::DeprecatedIndex) -> Self::DeprecatedKey {
        convert_column_index_to_key_bytes!(DeprecatedKey,
              ..8  => &index.to_be_bytes(),
             8..72 => signature.as_ref(),
            72..   => &slot.to_be_bytes(),
        )
    }

    fn try_deprecated_index(key: &[u8]) -> std::result::Result<Self::DeprecatedIndex, IndexError> {
        if key.len() != std::mem::size_of::<Self::DeprecatedKey>() {
            return Err(IndexError::UnpackError);
        }
        Ok(convert_column_key_bytes_to_index!(key,
             0..8  => u64::from_be_bytes,  // primary index
             8..72 => Signature::from,
            72..80 => Slot::from_be_bytes,
        ))
    }

    fn try_current_index(key: &[u8]) -> std::result::Result<Self::Index, IndexError> {
        if key.len() != Self::CURRENT_INDEX_LEN {
            return Err(IndexError::UnpackError);
        }
        Ok(convert_column_key_bytes_to_index!(key,
             0..64 => Signature::from,
            64..72 => Slot::from_be_bytes,
        ))
    }

    fn convert_index(deprecated_index: Self::DeprecatedIndex) -> Self::Index {
        let (_primary_index, signature, slot) = deprecated_index;
        (signature, slot)
    }
}

impl Column for columns::AddressSignatures {
    type Index = (Pubkey, Slot, /*transaction index:*/ u32, Signature);
    type Key = [u8; PUBKEY_BYTES
        + std::mem::size_of::<Slot>()
        + std::mem::size_of::<u32>()
        + SIGNATURE_BYTES];

    #[inline]
    fn key((pubkey, slot, transaction_index, signature): &Self::Index) -> Self::Key {
        convert_column_index_to_key_bytes!(Key,
              ..32 => pubkey.as_ref(),
            32..40 => &slot.to_be_bytes(),
            40..44 => &transaction_index.to_be_bytes(),
            44..   => signature.as_ref(),
        )
    }

    fn index(key: &[u8]) -> Self::Index {
        <columns::AddressSignatures as ColumnIndexDeprecation>::index(key)
    }

    fn slot(index: Self::Index) -> Slot {
        index.1
    }

    // The AddressSignatures column is not keyed by slot so this method is meaningless
    // See Column::as_index() declaration for more details
    fn as_index(_index: u64) -> Self::Index {
        (Pubkey::default(), 0, 0, Signature::default())
    }
}
impl ColumnName for columns::AddressSignatures {
    const NAME: &'static str = "address_signatures";
}

impl ColumnIndexDeprecation for columns::AddressSignatures {
    const CURRENT_INDEX_LEN: usize = 108;
    type DeprecatedIndex = (u64, Pubkey, Slot, Signature);
    type DeprecatedKey = [u8; 112];

    fn deprecated_key(
        (primary_index, pubkey, slot, signature): Self::DeprecatedIndex,
    ) -> Self::DeprecatedKey {
        convert_column_index_to_key_bytes!(DeprecatedKey,
              ..8  => &primary_index.to_be_bytes(),
             8..40 => pubkey.as_ref(),
            40..48 => &slot.to_be_bytes(),
            48..   => signature.as_ref(),
        )
    }

    fn try_deprecated_index(key: &[u8]) -> std::result::Result<Self::DeprecatedIndex, IndexError> {
        if key.len() != std::mem::size_of::<Self::DeprecatedKey>() {
            return Err(IndexError::UnpackError);
        }
        Ok(convert_column_key_bytes_to_index!(key,
             0..8   => u64::from_be_bytes,  // primary index
             8..40  => Pubkey::from,
            40..48  => Slot::from_be_bytes,
            48..112 => Signature::from,
        ))
    }

    fn try_current_index(key: &[u8]) -> std::result::Result<Self::Index, IndexError> {
        if key.len() != Self::CURRENT_INDEX_LEN {
            return Err(IndexError::UnpackError);
        }
        Ok(convert_column_key_bytes_to_index!(key,
             0..32  => Pubkey::from,
            32..40  => Slot::from_be_bytes,
            40..44  => u32::from_be_bytes,  // transaction index
            44..108 => Signature::from,
        ))
    }

    fn convert_index(deprecated_index: Self::DeprecatedIndex) -> Self::Index {
        let (_primary_index, pubkey, slot, signature) = deprecated_index;
        (pubkey, slot, 0, signature)
    }
}

impl Column for columns::TransactionMemos {
    type Index = (Signature, Slot);
    type Key = [u8; SIGNATURE_BYTES + std::mem::size_of::<Slot>()];

    #[inline]
    fn key((signature, slot): &Self::Index) -> Self::Key {
        convert_column_index_to_key_bytes!(Key,
            ..64 => signature.as_ref(),
            64.. => &slot.to_be_bytes(),
        )
    }

    fn index(key: &[u8]) -> Self::Index {
        <columns::TransactionMemos as ColumnIndexDeprecation>::index(key)
    }

    fn slot(index: Self::Index) -> Slot {
        index.1
    }

    fn as_index(index: u64) -> Self::Index {
        (Signature::default(), index)
    }
}
impl ColumnName for columns::TransactionMemos {
    const NAME: &'static str = "transaction_memos";
}

impl ColumnIndexDeprecation for columns::TransactionMemos {
    const CURRENT_INDEX_LEN: usize = 72;
    type DeprecatedIndex = Signature;
    type DeprecatedKey = [u8; 64];

    fn deprecated_key(signature: Self::DeprecatedIndex) -> Self::DeprecatedKey {
        Self::DeprecatedKey::from(signature)
    }

    fn try_deprecated_index(key: &[u8]) -> std::result::Result<Self::DeprecatedIndex, IndexError> {
        Signature::try_from(&key[..64]).map_err(|_| IndexError::UnpackError)
    }

    fn try_current_index(key: &[u8]) -> std::result::Result<Self::Index, IndexError> {
        if key.len() != Self::CURRENT_INDEX_LEN {
            return Err(IndexError::UnpackError);
        }
        Ok(convert_column_key_bytes_to_index!(key,
             0..64 => Signature::from,
            64..72 => Slot::from_be_bytes,
        ))
    }

    fn convert_index(deprecated_index: Self::DeprecatedIndex) -> Self::Index {
        (deprecated_index, 0)
    }
}

impl Column for columns::TransactionStatusIndex {
    type Index = u64;
    type Key = [u8; std::mem::size_of::<u64>()];

    #[inline]
    fn key(index: &Self::Index) -> Self::Key {
        index.to_be_bytes()
    }

    fn index(key: &[u8]) -> Self::Index {
        convert_column_key_bytes_to_index!(key, 0..8 => u64::from_be_bytes)
    }

    fn slot(_index: Self::Index) -> Slot {
        unimplemented!()
    }

    fn as_index(slot: u64) -> u64 {
        slot
    }
}
impl ColumnName for columns::TransactionStatusIndex {
    const NAME: &'static str = "transaction_status_index";
}

impl SlotColumn for columns::Rewards {}
impl ColumnName for columns::Rewards {
    const NAME: &'static str = "rewards";
}
impl ProtobufColumn for columns::Rewards {
    type Type = generated::Rewards;
}

impl SlotColumn for columns::Blocktime {}
impl ColumnName for columns::Blocktime {
    const NAME: &'static str = "blocktime";
}
impl TypedColumn for columns::Blocktime {
    type Type = UnixTimestamp;
}

impl SlotColumn for columns::PerfSamples {}
impl ColumnName for columns::PerfSamples {
    const NAME: &'static str = "perf_samples";
}

impl SlotColumn for columns::BlockHeight {}
impl ColumnName for columns::BlockHeight {
    const NAME: &'static str = "block_height";
}
impl TypedColumn for columns::BlockHeight {
    type Type = u64;
}

impl ColumnName for columns::ProgramCosts {
    const NAME: &'static str = "program_costs";
}
impl TypedColumn for columns::ProgramCosts {
    type Type = blockstore_meta::ProgramCost;
}
impl Column for columns::ProgramCosts {
    type Index = Pubkey;
    type Key = [u8; PUBKEY_BYTES];

    #[inline]
    fn key(pubkey: &Self::Index) -> Self::Key {
        pubkey.to_bytes()
    }

    fn index(key: &[u8]) -> Self::Index {
        convert_column_key_bytes_to_index!(key, 0..32 => Pubkey::from)
    }

    fn slot(_index: Self::Index) -> Slot {
        unimplemented!()
    }

    fn as_index(_index: u64) -> Self::Index {
        Pubkey::default()
    }
}

impl Column for columns::ShredCode {
    type Index = (Slot, /*shred index:*/ u64);
    type Key = <columns::ShredData as Column>::Key;

    #[inline]
    fn key(index: &Self::Index) -> Self::Key {
        // ShredCode and ShredData have the same key format
        <columns::ShredData as Column>::key(index)
    }

    fn index(key: &[u8]) -> Self::Index {
        columns::ShredData::index(key)
    }

    fn slot(index: Self::Index) -> Slot {
        index.0
    }

    fn as_index(slot: Slot) -> Self::Index {
        (slot, 0)
    }
}
impl ColumnName for columns::ShredCode {
    const NAME: &'static str = "code_shred";
}

impl Column for columns::ShredData {
    type Index = (Slot, /*shred index:*/ u64);
    type Key = [u8; std::mem::size_of::<Slot>() + std::mem::size_of::<u64>()];

    #[inline]
    fn key((slot, index): &Self::Index) -> Self::Key {
        convert_column_index_to_key_bytes!(Key,
            ..8 => &slot.to_be_bytes(),
            8.. => &index.to_be_bytes(),
        )
    }

    fn index(key: &[u8]) -> Self::Index {
        convert_column_key_bytes_to_index!(key,
            0..8  => Slot::from_be_bytes,
            8..16 => u64::from_be_bytes,  // shred index
        )
    }

    fn slot(index: Self::Index) -> Slot {
        index.0
    }

    fn as_index(slot: Slot) -> Self::Index {
        (slot, 0)
    }
}
impl ColumnName for columns::ShredData {
    const NAME: &'static str = "data_shred";
}

impl SlotColumn for columns::Index {}
impl ColumnName for columns::Index {
    const NAME: &'static str = "index";
}
impl TypedColumn for columns::Index {
    type Type = blockstore_meta::Index;

    fn deserialize(data: &[u8]) -> Result<Self::Type> {
        let config = bincode::DefaultOptions::new()
            // `bincode::serialize` uses fixint encoding by default, so we need to use the same here
            .with_fixint_encoding()
            .reject_trailing_bytes();

        // Migration strategy for new column format:
        // 1. Release 1: Add ability to read new format as fallback, keep writing old format
        // 2. Release 2: Switch to writing new format, keep reading old format as fallback
        // 3. Release 3: Remove old format support once stable
        // This allows safe downgrade to Release 1 since it can read both formats
        // https://github.com/anza-xyz/agave/issues/3570
        let index: bincode::Result<blockstore_meta::Index> = config.deserialize(data);
        match index {
            Ok(index) => Ok(index),
            Err(_) => {
                let index: blockstore_meta::IndexFallback = config.deserialize(data)?;
                Ok(index.into())
            }
        }
    }
}

impl SlotColumn for columns::DeadSlots {}
impl ColumnName for columns::DeadSlots {
    const NAME: &'static str = "dead_slots";
}
impl TypedColumn for columns::DeadSlots {
    type Type = bool;
}

impl SlotColumn for columns::DuplicateSlots {}
impl ColumnName for columns::DuplicateSlots {
    const NAME: &'static str = "duplicate_slots";
}
impl TypedColumn for columns::DuplicateSlots {
    type Type = blockstore_meta::DuplicateSlotProof;
}

impl SlotColumn for columns::Orphans {}
impl ColumnName for columns::Orphans {
    const NAME: &'static str = "orphans";
}
impl TypedColumn for columns::Orphans {
    type Type = bool;
}

impl SlotColumn for columns::BankHash {}
impl ColumnName for columns::BankHash {
    const NAME: &'static str = "bank_hashes";
}
impl TypedColumn for columns::BankHash {
    type Type = blockstore_meta::FrozenHashVersioned;
}

impl SlotColumn for columns::Root {}
impl ColumnName for columns::Root {
    const NAME: &'static str = "root";
}
impl TypedColumn for columns::Root {
    type Type = bool;
}

impl SlotColumn for columns::SlotMeta {}
impl ColumnName for columns::SlotMeta {
    const NAME: &'static str = "meta";
}
impl TypedColumn for columns::SlotMeta {
    type Type = blockstore_meta::SlotMeta;
}

impl Column for columns::ErasureMeta {
    type Index = (Slot, /*fec_set_index:*/ u64);
    type Key = [u8; std::mem::size_of::<Slot>() + std::mem::size_of::<u64>()];

    #[inline]
    fn key((slot, fec_set_index): &Self::Index) -> Self::Key {
        convert_column_index_to_key_bytes!(Key,
            ..8 => &slot.to_be_bytes(),
            8.. => &fec_set_index.to_be_bytes(),
        )
    }

    fn index(key: &[u8]) -> Self::Index {
        convert_column_key_bytes_to_index!(key,
            0..8  => Slot::from_be_bytes,
            8..16 => u64::from_be_bytes,  // fec_set_index
        )
    }

    fn slot(index: Self::Index) -> Slot {
        index.0
    }

    fn as_index(slot: Slot) -> Self::Index {
        (slot, 0)
    }
}
impl ColumnName for columns::ErasureMeta {
    const NAME: &'static str = "erasure_meta";
}
impl TypedColumn for columns::ErasureMeta {
    type Type = blockstore_meta::ErasureMeta;
}

impl SlotColumn for columns::OptimisticSlots {}
impl ColumnName for columns::OptimisticSlots {
    const NAME: &'static str = "optimistic_slots";
}
impl TypedColumn for columns::OptimisticSlots {
    type Type = blockstore_meta::OptimisticSlotMetaVersioned;
}

impl Column for columns::MerkleRootMeta {
    type Index = (Slot, /*fec_set_index:*/ u32);
    type Key = [u8; std::mem::size_of::<Slot>() + std::mem::size_of::<u32>()];

    #[inline]
    fn key((slot, fec_set_index): &Self::Index) -> Self::Key {
        convert_column_index_to_key_bytes!(Key,
            ..8 => &slot.to_be_bytes(),
            8.. => &fec_set_index.to_be_bytes(),
        )
    }

    fn index(key: &[u8]) -> Self::Index {
        convert_column_key_bytes_to_index!(key,
            0..8  => Slot::from_be_bytes,
            8..12 => u32::from_be_bytes,  // fec_set_index
        )
    }

    fn slot((slot, _fec_set_index): Self::Index) -> Slot {
        slot
    }

    fn as_index(slot: Slot) -> Self::Index {
        (slot, 0)
    }
}

impl ColumnName for columns::MerkleRootMeta {
    const NAME: &'static str = "merkle_root_meta";
}
impl TypedColumn for columns::MerkleRootMeta {
    type Type = blockstore_meta::MerkleRootMeta;
}
