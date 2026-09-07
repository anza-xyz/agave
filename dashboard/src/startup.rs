//! Publishing the validator's boot phase. Shared by the boot thread and the
//! collector, so the handover between them is invisible to the client.

use {
    crate::proto::{Debounced, Publisher, TOPIC_SUMMARY},
    serde::Serialize,
    solana_clock::Slot,
    solana_core::validator::ValidatorStartProgress,
    std::time::{Duration, Instant},
};

pub const KEY_STARTUP_PROGRESS: &str = "startup_progress";

/// What the client is sent about the boot sequence.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StartupProgress {
    /// Machine-readable phase name, e.g. `"loading_ledger"`.
    pub phase: String,
    pub detail: Option<String>,
    pub running: bool,
    /// How far ledger replay has got, from 0 to 1, measured from where it
    /// began.
    pub fraction: Option<f64>,
    /// Share of the cluster's stake seen in gossip while waiting for a
    /// supermajority, from 0 to 1.
    pub stake_percent: Option<f64>,
    /// How long the validator has been in this phase, and how long each phase
    /// before it took, since most phases cannot say how far along they are.
    pub phase_elapsed_nanos: u64,
    pub phases_taken: Vec<PhaseTiming>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PhaseTiming {
    pub phase: String,
    pub elapsed_nanos: u64,
}

#[derive(Default)]
pub struct StartupPublisher {
    debounce: Debounced<StartupProgress>,
    /// The first replay slot seen. Replay starts from a snapshot rather than
    /// from zero, so `slot / max_slot` would sit near 100% throughout.
    replay_origin: Option<Slot>,
    /// The phase being timed and when it began. The validator says which
    /// phase it is in and nothing about when it got there.
    current: Option<(String, Instant)>,
    /// How long each finished phase took. Accumulated if a phase comes round
    /// again, which `loading_ledger` does.
    taken: Vec<PhaseTiming>,
}

impl StartupPublisher {
    pub fn publish(&mut self, publisher: &Publisher, progress: ValidatorStartProgress) {
        let phase = describe(progress);
        let progress = StartupProgress {
            phase: phase.name.to_string(),
            detail: phase.detail,
            running: matches!(progress, ValidatorStartProgress::Running),
            fraction: self.fraction(phase.replay_slots),
            stake_percent: phase.stake_percent,
            phase_elapsed_nanos: self.elapsed(phase.name, Instant::now()),
            phases_taken: self.taken.clone(),
        };
        self.debounce
            .publish(publisher, TOPIC_SUMMARY, KEY_STARTUP_PROGRESS, progress);
    }

    /// Time in the current phase, rounded down to whole seconds so that the
    /// debounce does not send four messages a second for the length of a boot.
    fn elapsed(&mut self, phase: &str, now: Instant) -> u64 {
        match &mut self.current {
            Some((current, since)) if current == phase => whole_seconds(now, *since),
            other => {
                // A phase ending: keep what it took before starting the next.
                if let Some((finished, since)) = other.take() {
                    let elapsed_nanos = now.duration_since(since).as_nanos() as u64;
                    match self
                        .taken
                        .iter_mut()
                        .find(|timing| timing.phase == finished)
                    {
                        Some(timing) => {
                            timing.elapsed_nanos =
                                timing.elapsed_nanos.saturating_add(elapsed_nanos)
                        }
                        None => self.taken.push(PhaseTiming {
                            phase: finished,
                            elapsed_nanos,
                        }),
                    }
                }
                *other = Some((phase.to_string(), now));
                0
            }
        }
    }

    fn fraction(&mut self, slots: Option<(Slot, Slot)>) -> Option<f64> {
        let (current, target) = slots?;
        let origin = *self.replay_origin.get_or_insert(current);
        // Replay can be handed a target it has already passed, and the origin is
        // only an estimate; neither may run the meter backwards or past the end.
        let span = target.checked_sub(origin)?;
        if span == 0 {
            return Some(1.0);
        }
        let done = current.saturating_sub(origin).min(span);
        Some(done as f64 / span as f64)
    }
}

/// What a phase reports about itself, as far as it reports anything.
struct Phase {
    name: &'static str,
    detail: Option<String>,
    replay_slots: Option<(Slot, Slot)>,
    stake_percent: Option<f64>,
}

fn describe(progress: ValidatorStartProgress) -> Phase {
    let (name, detail, replay_slots, stake_percent) = match progress {
        ValidatorStartProgress::Initializing => ("initializing", None, None, None),
        ValidatorStartProgress::SearchingForRpcService => {
            ("searching_for_rpc_service", None, None, None)
        }
        ValidatorStartProgress::DownloadingSnapshot { slot, rpc_addr } => (
            "downloading_snapshot",
            Some(format!("slot {slot} from {rpc_addr}")),
            None,
            None,
        ),
        ValidatorStartProgress::CleaningBlockStore => ("cleaning_blockstore", None, None, None),
        ValidatorStartProgress::CleaningAccounts => ("cleaning_accounts", None, None, None),
        ValidatorStartProgress::LoadingLedger => ("loading_ledger", None, None, None),
        ValidatorStartProgress::ProcessingLedger { slot, max_slot } => (
            "processing_ledger",
            Some(format!("slot {slot} of {max_slot}")),
            Some((slot, max_slot)),
            None,
        ),
        ValidatorStartProgress::StartingServices => ("starting_services", None, None, None),
        ValidatorStartProgress::Halted => ("halted", None, None, None),
        ValidatorStartProgress::WaitingForSupermajority {
            slot,
            gossip_stake_percent,
        } => (
            "waiting_for_supermajority",
            Some(format!("slot {slot}")),
            None,
            Some(gossip_stake_percent as f64 / 100.0),
        ),
        ValidatorStartProgress::Running => ("running", None, None, None),
    };
    Phase {
        name,
        detail,
        replay_slots,
        stake_percent,
    }
}

/// A duration in whole seconds, as nanoseconds.
fn whole_seconds(now: Instant, since: Instant) -> u64 {
    Duration::from_secs(now.duration_since(since).as_secs()).as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replaying(slot: Slot, max_slot: Slot) -> ValidatorStartProgress {
        ValidatorStartProgress::ProcessingLedger { slot, max_slot }
    }

    #[test]
    fn test_fraction_is_measured_from_the_first_slot_seen() {
        let mut publisher = StartupPublisher::default();
        // Replay resumes from a snapshot at slot 1000 and is heading for 2000.
        assert_eq!(publisher.fraction(Some((1000, 2000))), Some(0.0));
        assert_eq!(publisher.fraction(Some((1500, 2000))), Some(0.5));
        assert_eq!(publisher.fraction(Some((2000, 2000))), Some(1.0));
    }

    #[test]
    fn test_fraction_is_absent_without_replay_slots() {
        let mut publisher = StartupPublisher::default();
        assert_eq!(publisher.fraction(None), None);
    }

    #[test]
    fn test_fraction_never_exceeds_one_or_runs_backwards() {
        let mut publisher = StartupPublisher::default();
        assert_eq!(publisher.fraction(Some((1000, 1100))), Some(0.0));
        // Overshooting the target must not report more than complete.
        assert_eq!(publisher.fraction(Some((1200, 1100))), Some(1.0));
        // Nor may a slot below the origin produce a negative fraction.
        assert_eq!(publisher.fraction(Some((900, 1100))), Some(0.0));
    }

    #[test]
    fn test_target_already_reached_reads_as_complete() {
        let mut publisher = StartupPublisher::default();
        assert_eq!(publisher.fraction(Some((1000, 1000))), Some(1.0));
    }

    #[test]
    fn test_target_behind_the_origin_reports_nothing() {
        let mut publisher = StartupPublisher::default();
        assert_eq!(publisher.fraction(Some((1000, 900))), None);
    }

    #[test]
    fn test_a_phase_is_timed_from_when_it_was_first_seen() {
        let mut publisher = StartupPublisher::default();
        let base = Instant::now();
        assert_eq!(publisher.elapsed("loading_ledger", base), 0);
        assert_eq!(
            publisher.elapsed("loading_ledger", base + Duration::from_millis(4_500)),
            Duration::from_secs(4).as_nanos() as u64,
            "rounded down to whole seconds, or the debounce sends four a second"
        );
    }

    #[test]
    fn test_a_finished_phase_keeps_what_it_took() {
        let mut publisher = StartupPublisher::default();
        let base = Instant::now();
        publisher.elapsed("cleaning_accounts", base);
        publisher.elapsed("loading_ledger", base + Duration::from_secs(30));

        assert_eq!(publisher.taken.len(), 1);
        assert_eq!(publisher.taken[0].phase, "cleaning_accounts");
        assert_eq!(
            publisher.taken[0].elapsed_nanos,
            Duration::from_secs(30).as_nanos() as u64
        );
    }

    #[test]
    fn test_a_phase_entered_twice_adds_to_its_own_total() {
        // `loading_ledger` is entered once for the snapshot and again if the
        // blockstore has slots to process.
        let mut publisher = StartupPublisher::default();
        let base = Instant::now();
        publisher.elapsed("loading_ledger", base);
        publisher.elapsed("processing_ledger", base + Duration::from_secs(10));
        publisher.elapsed("loading_ledger", base + Duration::from_secs(15));
        publisher.elapsed("processing_ledger", base + Duration::from_secs(21));

        let loading = publisher
            .taken
            .iter()
            .filter(|timing| timing.phase == "loading_ledger")
            .count();
        assert_eq!(
            loading, 1,
            "one row per phase, however often it comes round"
        );
        assert_eq!(
            publisher
                .taken
                .iter()
                .find(|timing| timing.phase == "loading_ledger")
                .unwrap()
                .elapsed_nanos,
            Duration::from_secs(16).as_nanos() as u64,
            "ten seconds the first time and six the second"
        );
    }

    #[test]
    fn test_publishing_fills_in_the_fraction() {
        let publisher = Publisher::new();
        let mut startup = StartupPublisher::default();
        startup.publish(&publisher, replaying(100, 200));
        startup.publish(&publisher, replaying(150, 200));

        let snapshot = publisher.snapshot();
        assert_eq!(
            snapshot.len(),
            1,
            "startup progress is a single retained key"
        );
        assert!(
            snapshot[0].contains(r#""fraction":0.5"#),
            "expected a half-complete fraction, got {}",
            snapshot[0]
        );
    }

    #[test]
    fn test_every_phase_has_a_name_and_running_is_the_only_running_one() {
        let phases = [
            ValidatorStartProgress::Initializing,
            ValidatorStartProgress::SearchingForRpcService,
            ValidatorStartProgress::CleaningBlockStore,
            ValidatorStartProgress::CleaningAccounts,
            ValidatorStartProgress::LoadingLedger,
            replaying(1, 2),
            ValidatorStartProgress::StartingServices,
            ValidatorStartProgress::Halted,
            ValidatorStartProgress::WaitingForSupermajority {
                slot: 5,
                gossip_stake_percent: 50,
            },
            ValidatorStartProgress::Running,
        ];
        for phase in phases {
            let publisher = Publisher::new();
            StartupPublisher::default().publish(&publisher, phase);
            let sent = publisher.snapshot().pop().unwrap();
            assert!(!sent.contains(r#""phase":"""#), "{sent}");
            assert_eq!(
                sent.contains(r#""running":true"#),
                phase == ValidatorStartProgress::Running,
                "{sent}"
            );
        }
    }
}
