#![no_std]

//! Messages passed between agave and an external pack process.
//! Messages are passed via `shaq::Consumer/Producer`.
//! Memory freeing is responsibility of the external pack process,
//! and is done via `rts-alloc` crate. It is also possible the external
//! pack process allocates memory to pass to agave, BUT it will still be
//! the responsibility of the external pack process to free that memory.

/// Reference to a transaction that can shared safely across processes.
#[repr(C)]
pub struct SharableTransaction {
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

/// Message: [TPU -> Pack]
/// TPU passes transactions to the external pack process.
/// This is also a transfer of ownership of the transaction:
///   the external pack process is responsible for freeing the memory.
pub type TpuToPackMessage = SharableTransaction;

/// Message: [Agave -> Pack]
/// Agave passes leader status to the external pack process.
#[repr(C)]
pub struct ProgressMessage {
    /// The slot the status is for.
    pub slot: u64,
    /// The current progress of the slot in percentage.
    /// Negative values indicate approximate time until the first leader slot
    /// begins.
    pub progress: i16,
    /// The number of compute units packed in the current block, if leader.
    pub total_compute_units: u64,
}

/// The maximum number of transactions that can be included in a single
/// [`PackToWorkerMessage`].
pub const MAX_TRANSACTIONS_PER_PACK_MESSAGE: usize = 32;

/// Special value for [`PackToWorkerMessage::slot`] indicating the transactions
/// can be processed in any slot in agave.
pub const ANY_SLOT: u64 = u64::MAX;

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
    /// If [`pack_message_flags::RESOLVE`] flag is not set, this is the slot
    /// the transactions can be processed in. If the flag is set to
    /// [`ANY_SLOT`], the transactions can be processed in any slot in agave.
    pub slot: u64,
    /// The number of transactions in this message.
    /// MUST be in the range [1, [`MAX_TRANSACTIONS_PER_PACK_MESSAGE`]].
    pub num_transactions: u16,
    /// Transactions in the message. Only the first `num_transactions`
    /// entries are valid to read.
    pub transactions: [SharableTransaction; MAX_TRANSACTIONS_PER_PACK_MESSAGE],
}

pub mod pack_message_flags {
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
    /// Tag indicating the type of message.
    /// See [`worker_message_types`] for details.
    pub tag: u8,
    /// The inner message, depending on the tag.
    /// See [`worker_message_types::WorkerToPackMessageInner`].
    pub inner: worker_message_types::WorkerToPackMessageInner,
}

pub mod worker_message_types {
    use {
        crate::{SharablePubkeys, SharableTransaction},
        core::mem::ManuallyDrop,
    };

    #[repr(C)]
    pub union WorkerToPackMessageInner {
        /// The message from pack was invalid.
        pub invalid: ManuallyDrop<InvalidMessage>,
        /// The transaction was not included in the block.
        pub not_included: ManuallyDrop<NotIncluded>,
        /// The transaction was included in the block.
        pub included: ManuallyDrop<Included>,
        /// The transaction was resolved.
        pub resolved: ManuallyDrop<Resolved>,
    }

    /// Tag indicating [`InvalidMessage`] inner message.
    pub const INVALID_MESSAGE: u8 = 0;

    /// Response to pack that a message was invalid.
    #[repr(C)]
    pub struct InvalidMessage;

    /// Tag indicating [`NotIncluded`] inner message.
    pub const NOT_INCLUDED: u8 = 1;

    /// Response to pack that a transaction was not included in the block.
    /// This response will only be sent if the original message tags
    /// requested execution i.e. not [`super::pack_message_flags::RESOLVE`].
    #[repr(C)]
    pub struct NotIncluded {
        /// The transaction that was not included.
        pub transaction: SharableTransaction,
        /// The reason the transaction was not included.
        /// See [`not_included_reasons`] for details.
        pub reason: u8,
    }

    pub mod not_included_reasons {
        // TODO: add reasons...(basically maps out `TransactionError`).
    }

    /// Tag indicating [`Included`] inner message.
    pub const INCLUDED: u8 = 2;

    /// Response to pack that a transaction was included in the block.
    /// This response will only be sent if the original message tags
    /// requested execution i.e. not [`super::pack_message_flags::RESOLVE`].
    #[repr(C)]
    pub struct Included {
        /// The transaction that was included.
        pub transaction: SharableTransaction,
        /// Compute units used by the transaction.
        pub compute_units: u64,
        /// The fee-payer balance after execution.
        pub fee_payer_balance: u64,
    }

    /// Tag indicating [`Resolved`] inner message.
    pub const RESOLVED: u8 = 3;

    #[repr(C)]
    pub struct Resolved {
        /// The transaction that was resolved.
        pub transaction: SharableTransaction,
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
