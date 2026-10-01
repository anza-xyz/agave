//! Observations of validator identity adoption, independent of consensus decisions.
//!
//! A submission is acceptance by the outbound voting channel, not delivery or finality.
//! State is owned by one validator and is never persisted in towers or vote histories.

use {
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Consensus context which acknowledged an identity change.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum IdentityTransitionConsensus {
    /// No authoritative consensus context is available.
    Unknown,
    /// Tower BFT replay voting.
    Tower,
    /// Alpenglow Votor voting.
    Alpenglow,
}

/// Progress of observation, without changing identity-command behavior.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum IdentityTransitionState {
    /// No identity command has been observed in this validator instance.
    Idle,
    /// The command or voting-loop acknowledgement is still pending.
    Transitioning,
    /// Both the command and voting-context adoption succeeded.
    Complete,
    /// A command failed or an authoritative observation cannot be supplied.
    Failed,
}

/// Snapshot returned identically by HTTP and admin RPC.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityTransitionStatus {
    /// Version of this observation contract.
    pub version: u64,
    /// Distinguishes validators and restarts; sequences are local to this instance.
    pub process_instance_id: String,
    /// Monotonic sequence of observed identity commands.
    pub sequence: u64,
    /// State of the latest observation.
    pub state: IdentityTransitionState,
    /// Consensus context of the observation.
    pub consensus: IdentityTransitionConsensus,
    /// Currently published identity; this can change before adoption completes.
    pub current_identity: String,
    /// Identity being replaced.
    pub from_identity: String,
    /// Requested identity.
    pub to_identity: String,
    /// Validator vote account, or an empty string before the first observation.
    pub vote_account: String,
    /// Highest outbound-channel-accepted slot in the old voting context.
    /// `None` means no submission was observed, not slot zero. Restored history
    /// is not a submission. This value is frozen when adoption is acknowledged.
    pub from_identity_last_submitted_vote_slot: Option<Slot>,
    /// Old Tower root when acknowledged by Tower; absent for Alpenglow.
    pub tower_root_slot: Option<Slot>,
    /// Command or observation failure, if any.
    pub error: Option<String>,
}

/// Thread-local accounting: no locks, allocation, persistence, or network work.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubmittedVoteSlots(Option<Slot>);

impl SubmittedVoteSlots {
    /// Observe a successful outbound-channel enqueue, including refreshes.
    #[inline]
    pub fn record(&mut self, slot: Slot) {
        self.0 = Some(self.0.map_or(slot, |previous| previous.max(slot)));
    }

    /// Highest submission observed in this identity's current voting context.
    #[inline]
    pub fn highest(&self) -> Option<Slot> {
        self.0
    }
}

#[derive(Clone)]
struct Record {
    sequence: u64,
    state: IdentityTransitionState,
    consensus: IdentityTransitionConsensus,
    from: Pubkey,
    to: Pubkey,
    vote_account: Pubkey,
    submitted: Option<Slot>,
    root: Option<Slot>,
    error: Option<Arc<str>>,
    command_finished: bool,
    adopted: bool,
}

#[derive(Default)]
struct Inner {
    record: Option<Record>,
    active_commands: usize,
    ambiguous: bool,
}

/// Validator-owned tracker. Only identity changes and queries take its lock.
pub struct IdentityTransitionTracker {
    process_instance_id: String,
    inner: Mutex<Inner>,
}

impl Default for IdentityTransitionTracker {
    fn default() -> Self {
        static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            process_instance_id: format!(
                "{}-{now}-{}",
                std::process::id(),
                NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
            ),
            inner: Mutex::new(Inner::default()),
        }
    }
}

impl IdentityTransitionTracker {
    /// Begin observing a validated command, before notifier or keypair updates.
    /// Overlaps disable authoritative completion until restart; commands are
    /// still accepted and executed by their original implementation.
    pub fn begin(
        &self,
        from: Pubkey,
        to: Pubkey,
        vote_account: Pubkey,
        consensus: IdentityTransitionConsensus,
        supported: bool,
    ) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.active_commands != 0
            || inner
                .record
                .as_ref()
                .is_some_and(|r| r.state == IdentityTransitionState::Transitioning)
        {
            inner.ambiguous = true;
        }
        let sequence = inner
            .record
            .as_ref()
            .map_or(1, |r| r.sequence.saturating_add(1));
        let error: Option<Arc<str>> = if inner.ambiguous {
            Some(
                "Overlapping identity changes: authoritative observation requires a restart".into(),
            )
        } else if !supported {
            Some("Identity transition observation is unavailable during consensus migration".into())
        } else if from == to {
            Some("Same-identity command has no identity-adoption barrier".into())
        } else if sequence == u64::MAX {
            Some("Identity transition sequence exhausted".into())
        } else {
            None
        };
        inner.active_commands = inner.active_commands.saturating_add(1);
        inner.record = Some(Record {
            sequence,
            state: if error.is_some() {
                IdentityTransitionState::Failed
            } else {
                IdentityTransitionState::Transitioning
            },
            consensus,
            from,
            to,
            vote_account,
            submitted: None,
            root: None,
            error,
            command_finished: false,
            adopted: false,
        });
        sequence
    }

    /// Finish observing the original command without altering its return value.
    pub fn finish_command(&self, sequence: u64, error: Option<String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.active_commands = inner.active_commands.saturating_sub(1);
        if let Some(record) = inner.record.as_mut().filter(|r| r.sequence == sequence) {
            record.command_finished = true;
            if let Some(error) = error {
                record.state = IdentityTransitionState::Failed;
                record.error = Some(error.into());
            } else if record.adopted && record.state == IdentityTransitionState::Transitioning {
                record.state = IdentityTransitionState::Complete;
            }
        }
    }

    /// Capture before reading the new identity keypair, preventing ABA matches.
    pub fn pending_sequence(&self) -> Option<u64> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record
            .as_ref()
            .filter(|r| r.state == IdentityTransitionState::Transitioning)
            .map(|r| r.sequence)
    }

    /// Acknowledge successful adoption with the old context's frozen watermark.
    pub fn acknowledge(
        &self,
        sequence: Option<u64>,
        from: Pubkey,
        to: Pubkey,
        consensus: IdentityTransitionConsensus,
        submitted: Option<Slot>,
        root: Option<Slot>,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(record) = inner.record.as_mut().filter(|r| {
            Some(r.sequence) == sequence
                && r.state == IdentityTransitionState::Transitioning
                && !r.adopted
        }) else {
            return;
        };
        if record.from != from || record.to != to || record.consensus != consensus {
            record.state = IdentityTransitionState::Failed;
            record.error =
                Some("Voting context does not match the requested identity transition".into());
            return;
        }
        record.submitted = submitted;
        record.root = root;
        record.adopted = true;
        if record.command_finished {
            record.state = IdentityTransitionState::Complete;
        }
    }

    /// Record a failure only for the captured observation, never a later one.
    pub fn fail(&self, sequence: Option<u64>, error: impl Into<String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = inner.record.as_mut().filter(|r| {
            Some(r.sequence) == sequence && r.state == IdentityTransitionState::Transitioning
        }) {
            record.state = IdentityTransitionState::Failed;
            record.error = Some(error.into().into());
        }
    }

    /// Copy typed state under the lock and format the response after releasing it.
    pub fn get(&self, current_identity: Pubkey) -> IdentityTransitionStatus {
        let record = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record
            .clone();
        IdentityTransitionStatus {
            version: 1,
            process_instance_id: self.process_instance_id.clone(),
            sequence: record.as_ref().map_or(0, |r| r.sequence),
            state: record
                .as_ref()
                .map_or(IdentityTransitionState::Idle, |r| r.state),
            consensus: record
                .as_ref()
                .map_or(IdentityTransitionConsensus::Unknown, |r| r.consensus),
            current_identity: current_identity.to_string(),
            from_identity: record
                .as_ref()
                .map_or(current_identity, |r| r.from)
                .to_string(),
            to_identity: record
                .as_ref()
                .map_or(current_identity, |r| r.to)
                .to_string(),
            vote_account: record
                .as_ref()
                .map_or_else(String::new, |r| r.vote_account.to_string()),
            from_identity_last_submitted_vote_slot: record.as_ref().and_then(|r| r.submitted),
            tower_root_slot: record.as_ref().and_then(|r| r.root),
            error: record
                .as_ref()
                .and_then(|r| r.error.as_ref().map(ToString::to_string)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (Pubkey, Pubkey, Pubkey) {
        (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        )
    }

    #[test]
    fn submissions_are_optional_and_monotonic() {
        let mut slots = SubmittedVoteSlots::default();
        assert_eq!(slots.highest(), None);
        slots.record(0);
        assert_eq!(slots.highest(), Some(0));
        slots.record(12);
        slots.record(9);
        slots.record(12);
        assert_eq!(slots.highest(), Some(12));
    }

    #[test]
    fn completion_requires_command_and_adoption_and_freezes_old_vote() {
        let (from, to, account) = ids();
        for command_first in [true, false] {
            let tracker = IdentityTransitionTracker::default();
            let seq = tracker.begin(from, to, account, IdentityTransitionConsensus::Tower, true);
            if command_first {
                tracker.finish_command(seq, None);
            }
            tracker.acknowledge(
                Some(seq),
                from,
                to,
                IdentityTransitionConsensus::Tower,
                Some(123),
                Some(90),
            );
            if !command_first {
                assert_eq!(
                    tracker.get(to).state,
                    IdentityTransitionState::Transitioning
                );
                tracker.finish_command(seq, None);
            }
            assert_eq!(tracker.get(to).state, IdentityTransitionState::Complete);
            tracker.acknowledge(
                Some(seq),
                from,
                to,
                IdentityTransitionConsensus::Tower,
                Some(999),
                Some(100),
            );
            assert_eq!(
                tracker.get(to).from_identity_last_submitted_vote_slot,
                Some(123)
            );
        }
    }

    #[test]
    fn passive_context_has_no_submitted_vote() {
        let (from, to, account) = ids();
        let tracker = IdentityTransitionTracker::default();
        let seq = tracker.begin(
            from,
            to,
            account,
            IdentityTransitionConsensus::Alpenglow,
            true,
        );
        tracker.finish_command(seq, None);
        tracker.acknowledge(
            Some(seq),
            from,
            to,
            IdentityTransitionConsensus::Alpenglow,
            None,
            None,
        );
        assert_eq!(tracker.get(to).state, IdentityTransitionState::Complete);
        assert_eq!(tracker.get(to).from_identity_last_submitted_vote_slot, None);
    }

    #[test]
    fn failures_and_overlaps_never_complete() {
        let (from, to, account) = ids();
        let tracker = IdentityTransitionTracker::default();
        let first = tracker.begin(from, to, account, IdentityTransitionConsensus::Tower, true);
        tracker.finish_command(first, None);
        let second = tracker.begin(from, to, account, IdentityTransitionConsensus::Tower, true);
        assert!(second > first);
        tracker.finish_command(second, None);
        tracker.acknowledge(
            Some(first),
            from,
            to,
            IdentityTransitionConsensus::Tower,
            Some(1),
            None,
        );
        tracker.acknowledge(
            Some(second),
            from,
            to,
            IdentityTransitionConsensus::Tower,
            Some(2),
            None,
        );
        assert_eq!(tracker.get(to).state, IdentityTransitionState::Failed);
        assert_eq!(tracker.get(to).from_identity_last_submitted_vote_slot, None);
    }

    #[test]
    fn command_failure_after_adoption_is_terminal() {
        let (from, to, account) = ids();
        let tracker = IdentityTransitionTracker::default();
        let seq = tracker.begin(from, to, account, IdentityTransitionConsensus::Tower, true);
        tracker.acknowledge(
            Some(seq),
            from,
            to,
            IdentityTransitionConsensus::Tower,
            Some(42),
            Some(10),
        );
        tracker.finish_command(seq, Some("event channel closed".into()));
        assert_eq!(tracker.get(to).state, IdentityTransitionState::Failed);
    }

    #[test]
    fn mismatched_context_and_migration_fail_closed() {
        let (from, to, account) = ids();
        for mismatch in 0..3 {
            let tracker = IdentityTransitionTracker::default();
            let seq = tracker.begin(from, to, account, IdentityTransitionConsensus::Tower, true);
            tracker.finish_command(seq, None);
            tracker.acknowledge(
                Some(seq),
                if mismatch == 0 { account } else { from },
                if mismatch == 1 { account } else { to },
                if mismatch == 2 {
                    IdentityTransitionConsensus::Alpenglow
                } else {
                    IdentityTransitionConsensus::Tower
                },
                Some(1),
                None,
            );
            assert_eq!(tracker.get(to).state, IdentityTransitionState::Failed);
        }
        let tracker = IdentityTransitionTracker::default();
        let seq = tracker.begin(
            from,
            to,
            account,
            IdentityTransitionConsensus::Unknown,
            false,
        );
        tracker.finish_command(seq, None);
        assert_eq!(tracker.get(to).state, IdentityTransitionState::Failed);
    }

    #[test]
    fn instances_and_restarts_are_isolated() {
        let (from, to, account) = ids();
        let one = IdentityTransitionTracker::default();
        let two = IdentityTransitionTracker::default();
        one.begin(from, to, account, IdentityTransitionConsensus::Tower, true);
        assert_ne!(
            one.get(from).process_instance_id,
            two.get(from).process_instance_id
        );
        assert_eq!(two.get(to).sequence, 0);
        assert_eq!(two.get(to).state, IdentityTransitionState::Idle);
    }

    #[test]
    fn overlapping_threads_cannot_claim_completion() {
        let (from, to, account) = ids();
        let tracker = Arc::new(IdentityTransitionTracker::default());
        let barrier = Arc::new(std::sync::Barrier::new(4));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let tracker = tracker.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let seq =
                        tracker.begin(from, to, account, IdentityTransitionConsensus::Tower, true);
                    barrier.wait();
                    tracker.finish_command(seq, None);
                    tracker.acknowledge(
                        Some(seq),
                        from,
                        to,
                        IdentityTransitionConsensus::Tower,
                        Some(10),
                        None,
                    );
                });
            }
        });
        assert_eq!(tracker.get(to).state, IdentityTransitionState::Failed);
        assert_eq!(tracker.get(to).from_identity_last_submitted_vote_slot, None);
    }

    #[test]
    fn stale_acknowledgements_do_not_change_a_later_transition() {
        let (from, to, account) = ids();
        let tracker = IdentityTransitionTracker::default();
        let first = tracker.begin(from, to, account, IdentityTransitionConsensus::Tower, true);
        tracker.finish_command(first, None);
        tracker.acknowledge(
            Some(first),
            from,
            to,
            IdentityTransitionConsensus::Tower,
            Some(10),
            Some(0),
        );
        let second = tracker.begin(to, from, account, IdentityTransitionConsensus::Tower, true);
        tracker.acknowledge(
            Some(first),
            from,
            to,
            IdentityTransitionConsensus::Tower,
            Some(99),
            Some(0),
        );
        assert_eq!(tracker.pending_sequence(), Some(second));
        tracker.finish_command(second, None);
        tracker.acknowledge(
            Some(second),
            to,
            from,
            IdentityTransitionConsensus::Tower,
            None,
            Some(0),
        );
        assert_eq!(tracker.get(from).state, IdentityTransitionState::Complete);
        assert_eq!(
            tracker.get(from).from_identity_last_submitted_vote_slot,
            None
        );
    }

    #[test]
    fn same_identity_and_adoption_failure_do_not_claim_completion() {
        let (from, to, account) = ids();
        let tracker = IdentityTransitionTracker::default();
        let seq = tracker.begin(
            from,
            from,
            account,
            IdentityTransitionConsensus::Tower,
            true,
        );
        tracker.finish_command(seq, None);
        assert_eq!(tracker.get(from).state, IdentityTransitionState::Failed);
        let next = tracker.begin(from, to, account, IdentityTransitionConsensus::Tower, true);
        tracker.fail(Some(seq), "stale failure");
        assert_eq!(
            tracker.get(from).state,
            IdentityTransitionState::Transitioning
        );
        tracker.fail(Some(next), "tower load failed");
        tracker.finish_command(next, None);
        tracker.acknowledge(
            Some(next),
            from,
            to,
            IdentityTransitionConsensus::Tower,
            Some(1),
            None,
        );
        assert_eq!(tracker.get(to).state, IdentityTransitionState::Failed);
    }
}
