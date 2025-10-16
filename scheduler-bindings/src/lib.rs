#![no_std]

//! Messages passed between agave and an external pack process.
//! Messages are passed via `shaq::Consumer/Producer`.
//!
//! Memory freeing is responsibility of the external pack process,
//! and is done via `rts-alloc` crate. It is also possible the external
//! pack process allocates memory to pass to agave, BUT it will still be
//! the responsibility of the external pack process to free that memory.
//!
//! Setting up the shared memory allocator and queues is done outside of
//! agave - it can be done by the external pack process or another
//! process. agave will just `join` shared memory regions, but not
//! create them.
//! Similarly, agave will not delete files used for shared memory regions.
//! See `shaq` and `rts-alloc` crates for details.
//!
//! The basic architecture is as follows:
//!        ┌───────────────┐       ┌─────────────────┐
//!        │  tpu_to_pack  │       │ progress_tracker│
//!        └───────┬───────┘       └───────┬─────────┘
//!                │                       │
//!                │                       │
//!                │                       │
//!             ┌──▼───────────────────────▼───┐
//!             │  external scheduler          │
//!             └─▲─────── ▲────────────────▲──┘
//!               │        │                │
//!               │        │                │
//!           ┌───▼───┐ ┌──▼─────┐ ...  ┌───▼───┐
//!           │worker1│ │worker2 │      │workerN│
//!           └───────┘ └────────┘      └───────┘
//!
//! - [`TpuToPackMessage`] are sent from `tpu_to_pack` queue to the
//!   external scheduler process. This passes in tpu transactions to be scheduled,
//!   and optionally vote transactions.
//! - [`ProgressMessage`] are sent from `progress_tracker` queue to the
//!   external scheduler process. This passes information about leader status
//!   and slot progress to the external scheduler process.
//! - [`PackToWorkerMessage`] are sent from the external scheduler process
//!   to worker threads within agave. This passes a batch of transactions
//!   to be processed by the worker threads. This processing can also involve
//!   resolving the transactions' addresses, or similar operations beyond
//!   execution.
//! - [`WorkerToPackMessage`] are sent from worker threads within agave
//!   back to the external scheduler process. This passes back the results
//!   of processing the transactions.
//!

/// Reference to a transaction that can shared safely across processes.
#[repr(C)]
pub struct SharableTransactionRegion {
    /// Offset within the shared memory allocator.
    pub offset: usize,
    /// Length of the transaction in bytes.
    pub length: u32,
}

/// Reference to an array of Pubkeys that can be shared safely across processes.
#[repr(C)]
pub struct SharablePubkeys {
    /// Offset within the shared memory allocator.
    pub offset: usize,
    /// Number of pubkeys in the array.
    /// IF 0, indicates no pubkeys and no allocation needing to be freed.
    pub num_pubkeys: u32,
}

/// Reference to an array of [`SharableTransactionRegion`] that can be shared safely
/// across processes.
/// General flow:
/// 1. External pack process allocates memory for
///    `num_transactions` [`SharableTransactionRegion`].
/// 2. External pack sends a [`PackToWorkerMessage`] with `batch`.
/// 3. agave processes the transactions and sends back a [`WorkerToPackMessage`]
///    with the same `batch`.
/// 4. External pack process frees all transaction memory pointed to by the
///    [`SharableTransactionRegion`] in the batch, then frees the memory for
///    the array of [`SharableTransactionRegion`].
#[repr(C)]
pub struct SharableTransactionBatchRegion {
    /// Number of transactions in the batch.
    pub num_transactions: u8,
    /// Offset within the shared memory allocator for the batch of transactions.
    /// The transactions are laid out back-to-back in memory as a
    /// [`SharableTransactionRegion`] with size `num_transactions`.
    pub transactions_offset: u32,
}
/// Reference to an array of response messages.
/// General flow:
/// 1. agave allocates memory for `num_transaction_responses` inner messages.
/// 2. agave sends a [`WorkerToPackMessage`] with `responses`.
/// 3. External pack process processes the inner messages. Potentially freeing
///    any memory within each inner message (see [`worker_message_types`] for details).
#[repr(C)]
pub struct TransactionResponseRegion {
    /// Tag indicating the type of message.
    /// See [`worker_message_types`] for details.
    /// All inner messages/responses per trasaction will be of the same type.
    pub tag: u8,
    /// The number of transactions in the original message.
    /// This corresponds to the number of inner response
    /// messages that will be pointed to by `response_offset`.
    /// This MUST be the same as `batch.num_transactions`.
    pub num_transaction_responses: u8,
    /// Offset within the shared memory allocator for the array of
    /// inner messages.
    /// The inner messages are laid out back-to-back in memory starting at
    /// this offset. The type of each inner message is indicated by `tag`.
    /// There are `num_transaction_responses` inner messages.
    /// See [`worker_message_types`] for details on the inner message types.
    pub transaction_responses_offset: u32,
}

/// Message: [TPU -> Pack]
/// TPU passes transactions to the external pack process.
/// This is also a transfer of ownership of the transaction:
///   the external pack process is responsible for freeing the memory.
#[repr(C)]
pub struct TpuToPackMessage {
    pub transaction: SharableTransactionRegion,
    /// See [`tpu_message_flags`] for details.
    pub flags: u8,
    /// The source address of the transaction.
    /// IPv6-mapped IPv4 addresses: `::ffff:a.b.c.d`
    /// where a.b.c.d is the IPv4 address.
    /// See <https://datatracker.ietf.org/doc/html/rfc4291#section-2.5.5.2>.
    pub src_addr: [u8; 16],
}

pub mod tpu_message_flags {
    /// No special flags.
    pub const NONE: u8 = 0;

    /// The transaction is a simple vote transaction.
    pub const IS_SIMPLE_VOTE: u8 = 1 << 0;
    /// The transaction was forwarded by a validator node.
    pub const FORWARDED: u8 = 1 << 1;
    /// The transaction was sent from a staked node.
    pub const FROM_STAKED_NODE: u8 = 1 << 2;
}

/// Message: [Agave -> Pack]
/// Agave passes leader status to the external pack process.
#[repr(C)]
pub struct ProgressMessage {
    /// The current slot.
    pub current_slot: u64,
    /// Next known leader slot or u64::MAX if unknown.
    /// If currently leader, this is equal to `current_slot`.
    pub next_leader_slot: u64,
    /// The remaining cost units allowed to be packed in the block.
    /// i.e. block_limit - current_cost_units_used.
    /// Only valid if currently leader, otherwise the value is undefined.
    pub remaining_cost_units: u64,
    /// Progress through the current slot in percentage.
    pub current_slot_progress: u8,
}

/// Maximum number of transactions allowed in a [`PackToWorkerMessage`].
/// If the number of transactions exceeds this value, agave will
/// not process the message.
//
// The reason for this constraint is because rts-alloc currently only
// supports up to 4096 byte allocations. We must ensure that the
// `TransactionResponseRegion` is able to contain responses for all
// transactions sent. This is a conservative bound.
pub const MAX_TRANSACTIONS_PER_MESSAGE: usize = 64;

/// Message: [Pack -> Worker]
/// External pack processe passes transactions to worker threads within agave.
///
/// These messages do not transfer ownership of the transactions.
/// The external pack process is still responsible for freeing the memory.
#[repr(C)]
pub struct PackToWorkerMessage {
    /// Flags on how to handle this message.
    /// See [`pack_message_flags`] for details.
    pub flags: u16,
    /// If [`pack_message_flags::RESOLVE`] flag is not set, this is the
    /// maximum slot the transactions can be processed in. If the working
    /// bank's slot in the worker thread is greater than this slot,
    /// the transaction will not be processed.
    pub max_execution_slot: u64,
    /// Offset and number of transactions in the batch.
    /// See [`SharableTransactionBatchRegion`] for details.
    /// Agave will return this batch in the response message, it is
    /// the responsibility of the external pack process to free the memory
    /// ONLY after receiving the response message.
    pub batch: SharableTransactionBatchRegion,
}

pub mod pack_message_flags {
    //! Flags for [`crate::PackToWorkerMessage::flags`].
    //! These flags can be ORed together so must be unique bits, with
    //! the exception of [`NONE`].
    //! The *default* behavior, [`NONE`], is to attempt execution and
    //! inclusion in the specified `max_execution_slot`.

    /// No special handling - execute the transactions normally.
    pub const NONE: u16 = 0;

    /// Transactions on the [`super::PackToWorkerMessage`] should have their
    /// addresses resolved.
    ///
    /// If this flag, the transaction will attempt to be executed and included
    /// in the current block.
    pub const RESOLVE: u16 = 1 << 1;
}

/// Message: [Worker -> Pack]
/// Message from worker threads in response to a [`PackToWorkerMessage`].
/// [`PackToWorkerMessage`] may have multiple response messages that
/// will follow the order of transactions in the original message.
#[repr(C)]
pub struct WorkerToPackMessage {
    /// Offset and number of transactions in the batch.
    /// See [`SharableTransactionBatchRegion`] for details.
    /// Once the external pack process receives this message,
    /// it is responsible for freeing the memory for this batch,
    /// and is safe to do so - agave will hold no references to this memory
    /// after sending this message.
    pub batch: SharableTransactionBatchRegion,
    /// `true` if the message was processed.
    /// `false` if the message could not be processed. This will occur
    /// if the passed message was invalid, and could indicate an issue
    /// with the external pack process.
    /// If `false`, the value of [`Self::responses`] is undefined.
    pub processed: bool,
    /// Response per transaction in the batch.
    /// See [`TransactionResponseRegion`] for details.
    pub responses: TransactionResponseRegion,
}

pub mod worker_message_types {
    use crate::SharablePubkeys;

    /// Tag indicating [`ExecutionResonse`] inner message.
    pub const EXECUTION_RESPONSE: u8 = 0;

    /// Response to pack for a transaction that attempted execution.
    /// This response will only be sent if the original message flags
    /// requested execution i.e. not [`super::pack_message_flags::RESOLVE`].
    #[repr(C)]
    pub struct ExecutionResponse {
        /// Indicates if the transaction was included in the block or not.
        /// If [`not_included_reasons::NONE`], the transaction was included.
        pub not_included_reason: u8,
        /// If included, cost units used by the transaction.
        pub cost_units: u64,
        /// If included, the fee-payer balance after execution.
        pub fee_payer_balance: u64,
    }

    pub mod not_included_reasons {
        /// The transaction was included in the block.
        pub const NONE: u8 = 0;
        /// The transaction could not attempt processing because the
        /// working bank was unavailable.
        pub const BANK_NOT_AVAILABLE: u8 = 1;
        /// The transaction could not be processed because the `slot`
        /// in the passed message did not match the working bank's slot.
        pub const SLOT_MISMATCH: u8 = 2;

        // The following reasons are mapped from `TransactionError` in
        // `solana-sdk` crate. See that crate for details.
        pub const PARSING_OR_SANITIZATION_FAILURE: u8 = 3;
        pub const ALT_RESOLUTION_FAILURE: u8 = 4;
        pub const BLOCKHASH_NOT_FOUND: u8 = 5;
        pub const ALREADY_PROCESSED: u8 = 6;
        pub const WOULD_EXCEED_VOTE_MAX_LIMIT: u8 = 7;
        pub const WOULD_EXCEED_BLOCK_MAX_LIMIT: u8 = 8;
        pub const WOULD_EXCEED_ACCOUNT_MAX_LIMIT: u8 = 9;
        pub const WOULD_EXCEED_ACCOUNT_DATA_BLOCK_LIMIT: u8 = 10;
        pub const TOO_MANY_ACCOUNT_LOCKS: u8 = 11;
        pub const ACCOUNT_LOADED_TWICE: u8 = 12;
        pub const ACCOUNT_IN_USE: u8 = 13;
        pub const INVALID_ACCOUNT_FOR_FEE: u8 = 14;
        pub const INSUFFICIENT_FUNDS_FOR_FEE: u8 = 15;
        pub const INSUFFICIENT_FUNDS_FOR_RENT: u8 = 16;
    }

    /// Tag indicating [`Resolved`] inner message.
    pub const RESOLVED: u8 = 1;

    #[repr(C)]
    pub struct Resolved {
        /// Indicates if resolution was successful.
        pub success: bool,
        /// Slot of the bank used for resolution.
        pub slot: u64,
        /// Minimum deactivation slot of any ALT if any.
        /// u64::MAX if no ALTs or deactivation.
        pub min_alt_deactivation_slot: u64,
        /// Resolved pubkeys - writable then readonly.
        /// Freeing this memory is the responsiblity of the external
        /// pack process.
        pub resolved_pubkeys: SharablePubkeys,
    }
}
