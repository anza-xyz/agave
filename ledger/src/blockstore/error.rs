//! The error that can be produced from Blockstore operations.

use {
    super::PurgeType, crate::blockstore_meta::BlockLocation,
    agave_snapshots::hardened_unpack::UnpackError, solana_clock::Slot, thiserror::Error,
};

#[derive(Error, Debug)]
pub enum BlockstoreError {
    #[error("shred for index exists")]
    ShredForIndexExists,
    #[error("invalid shred data")]
    InvalidShredData(bincode::Error),
    #[error("RocksDB error: {0}")]
    RocksDb(#[from] rocksdb::Error),
    #[error("slot is not rooted")]
    SlotNotRooted,
    #[error("dead slot")]
    DeadSlot,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("deserialization error: {0}")]
    Deserialize(#[from] wincode::ReadError),
    #[error("serialization error: {0}")]
    Serialize(#[from] bincode::Error),
    #[error("fs extra error: {0}")]
    FsExtraError(#[from] fs_extra::error::Error),
    #[error("slot cleaned up")]
    SlotCleanedUp,
    #[error("unpack error: {0}")]
    UnpackError(#[from] UnpackError),
    #[error("transaction status slot mismatch")]
    TransactionStatusSlotMismatch,
    #[error("empty epoch stakes")]
    EmptyEpochStakes,
    #[error("no vote timestamps in range")]
    NoVoteTimestampsInRange,
    #[error("protobuf encode error: {0}")]
    ProtobufEncodeError(#[from] prost::EncodeError),
    #[error("protobuf decode error: {0}")]
    ProtobufDecodeError(#[from] prost::DecodeError),
    #[error("parent entries unavailable")]
    ParentEntriesUnavailable,
    #[error("parent info unavailable: slot {0} location {1}")]
    ParentInfoUnavailable(Slot, BlockLocation),
    #[error("merkle tree construction failure: slot {0} location {1}")]
    MerkleTreeConstructionFailure(Slot, BlockLocation),
    #[error("merkle proof construction failure: slot {0} location {1}")]
    MerkleProofConstructionFailure(Slot, BlockLocation),
    #[error("slot unavailable")]
    SlotUnavailable,
    #[error("unsupported transaction version")]
    UnsupportedTransactionVersion,
    #[error("missing transaction metadata")]
    MissingTransactionMetadata,
    #[error("transaction-index overflow")]
    TransactionIndexOverflow,
    #[error("invalid erasure config")]
    InvalidErasureConfig,
    #[error("last shred index missing slot {0}")]
    UnknownLastIndex(Slot),
    #[error("missing shred slot {0}, index {1}")]
    MissingShred(Slot, u64),
    #[error("legacy shred slot {0}, index {1}")]
    LegacyShred(Slot, u64),
    #[error("unable to read merkle root slot {0}, index {1}")]
    MissingMerkleRoot(Slot, u64),
    #[error("unable to purge slots in range [{from_slot}, {to_slot}] {purge_type:?}: {inner:?}")]
    PurgeFailed {
        from_slot: Slot,
        to_slot: Slot,
        purge_type: PurgeType,
        #[source]
        inner: Box<BlockstoreError>,
    },
    #[error(transparent)]
    ManualPurge(#[from] BlockstoreManualPurgeError),
}
pub type Result<T> = std::result::Result<T, BlockstoreError>;

#[derive(Error, Debug)]
pub enum BlockstoreManualPurgeError {
    #[error("purge request sender is unavailable")]
    SenderUnavailable,

    #[error("purge request for slot {request_slot} is newer than the latest root {max_root}")]
    SlotNewerThanRoot { request_slot: Slot, max_root: Slot },

    #[error("purge request try send error")]
    TrySend,
}

impl<T> std::convert::From<crossbeam_channel::TrySendError<T>> for BlockstoreManualPurgeError {
    fn from(_e: crossbeam_channel::TrySendError<T>) -> BlockstoreManualPurgeError {
        BlockstoreManualPurgeError::TrySend
    }
}
