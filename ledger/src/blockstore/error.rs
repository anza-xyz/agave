//! The error that can be produced from Blockstore operations.

use {agave_snapshots::hardened_unpack::UnpackError, solana_clock::Slot, thiserror::Error};

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
}
pub type Result<T> = std::result::Result<T, BlockstoreError>;
