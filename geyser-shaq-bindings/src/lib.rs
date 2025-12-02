#![no_std]

//! Messages passed between agave validator and geyser consumers.
//! Messages are passed via `shaq::Consumer/Producer`.
//!
//! Memory management is responsibility of the consumer process,
//! using the `rts-alloc` crate.

/// Account update message passed via shared memory queue
#[repr(C)]
#[derive(Clone, Debug)]
pub struct AccountUpdateMessage {
    pub slot: u64,
    pub write_version: u64,
    pub lamports: u64,
    pub rent_epoch: u64,
    pub pubkey: [u8; 32],
    pub owner: [u8; 32],
    pub executable: bool,
    pub is_startup: bool,
    // For data, use shared memory offset/length
    pub data: SharableDataRegion,
    // Optional transaction signature (0 if none)
    pub txn_signature: [u8; 64],
    pub has_txn: bool,
}

#[repr(C)]
pub enum AccountData {
    /// Data stored inline in the message (up to 256 bytes) ? need this?
    Inline { length: u32, data: [u8; 256] },
    /// Data stored in shared memory (for larger accounts)
    SharedMemory { offset: usize, length: u32 },
}

/// Reference to data in shared memory allocator
#[repr(C)]
#[derive(Clone, Debug)]
pub struct SharableDataRegion {
    pub offset: usize,
    pub length: u32,
}

/// Transaction notification message
#[repr(C)]
pub struct TransactionMessage {
    pub slot: u64,
    pub index: usize,
    pub signature: [u8; 64],
    pub is_vote: bool,
    // Transaction data in shared memory
    pub transaction: SharableTransactionRegion,
    pub status: TransactionStatus,
    pub compute_units_consumed: u64,
}

#[repr(C)]
pub struct TransactionStatus {
    pub success: bool,
    pub error_code: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct SharableTransactionRegion {
    pub offset: usize,
    pub length: u32,
}

/// Slot status update message
#[repr(C)]
#[derive(Debug)]
pub struct SlotStatusMessage {
    pub slot: u64,
    pub parent: u64,
    pub has_parent: bool,
    pub status: SlotStatusCode,
    pub timestamp_ns: u64,
}

#[repr(u8)]
#[derive(Debug)]
pub enum SlotStatusCode {
    Processed = 0,
    Confirmed = 1,
    Rooted = 2,
    FirstShredReceived = 3,
    Completed = 4,
    CreatedBank = 5,
    Dead = 6,
}

/// Block metadata message
#[repr(C)]
#[derive(Debug)]
pub struct BlockMetadataMessage {
    pub slot: u64,
    pub parent_slot: u64,
    pub blockhash: [u8; 32],
    pub parent_blockhash: [u8; 32],
    pub block_height: u64,
    pub block_time: i64,
    pub has_block_time: bool,
    pub executed_transaction_count: u64,
    pub entry_count: u64,
    // Rewards in shared memory
    pub rewards: SharableRewardsRegion,
}

#[repr(C)]
#[derive(Debug)]
pub struct SharableRewardsRegion {
    pub offset: usize,
    pub count: u32,
}

/// Entry notification message
#[repr(C)]
#[derive(Debug)]
pub struct EntryMessage {
    pub slot: u64,
    pub index: usize,
    pub num_hashes: u64,
    pub hash: [u8; 32],
    pub executed_transaction_count: u64,
    pub starting_transaction_index: usize,
}
