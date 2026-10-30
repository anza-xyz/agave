use {
    agave_event_system::{
        event,
        event_metadata::{cached_thread_and_process_id, monotonic_timestamp_ns},
        stream_name::StreamName,
    },
    solana_clock::{BankId, Slot},
};

pub const SLOT_EVENT_STREAM: StreamName = agave_event_system::stream_name!("replay.slot_event");

/// Slot lifecycle events observed by replay.
/// All timestamps are monotonic nanoseconds.
#[event]
#[derive(Debug, PartialEq, Eq)]
pub enum SlotEvent {
    /// Replay inserted a bank for this slot and parent into BankForks.
    /// The slot may begin again if its bank is discarded and recreated.
    Begin {
        pid: i32,
        tid: i32,
        timestamp_ns: u64,
        slot: Slot,
        parent: Slot,
        bank_id: BankId,
    },
    /// Execution stopped for this bank; `succeeded` records replay and verification success.
    ExecutionFinished {
        pid: i32,
        tid: i32,
        timestamp_ns: u64,
        slot: Slot,
        bank_id: BankId,
        succeeded: bool,
    },
    /// The bank was removed from BankForks, but may still have other owners.
    Removed {
        pid: i32,
        tid: i32,
        timestamp_ns: u64,
        slot: Slot,
        bank_id: BankId,
    },
    /// Replay applied a BankForks root that includes this bank.
    Rooted {
        pid: i32,
        tid: i32,
        timestamp_ns: u64,
        slot: Slot,
        bank_id: BankId,
    },
}

impl SlotEvent {
    pub(crate) fn execution_finished(slot: Slot, bank_id: BankId, succeeded: bool) -> Self {
        let (tid, pid) = cached_thread_and_process_id();
        Self::ExecutionFinished {
            pid,
            tid,
            timestamp_ns: monotonic_timestamp_ns(),
            slot,
            bank_id,
            succeeded,
        }
    }

    pub(crate) fn rooted(slot: Slot, bank_id: BankId) -> Self {
        let (tid, pid) = cached_thread_and_process_id();
        Self::Rooted {
            pid,
            tid,
            timestamp_ns: monotonic_timestamp_ns(),
            slot,
            bank_id,
        }
    }

    pub(crate) fn removed(slot: Slot, bank_id: BankId) -> Self {
        let (tid, pid) = cached_thread_and_process_id();
        Self::Removed {
            pid,
            tid,
            timestamp_ns: monotonic_timestamp_ns(),
            slot,
            bank_id,
        }
    }
}
