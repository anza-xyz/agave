use {
    solana_pubkey::Pubkey,
    solana_time_utils::timestamp,
    std::{
        collections::HashMap,
        time::{Duration, Instant},
    },
};

/// Immutable inputs and timestamps shared by one transmit-side gossip round.
///
/// Wall-clock time is sampled once so that every CRDS timestamp written by the
/// round agrees. Scheduling and sleep calculations use `Instant`, so a system
/// clock adjustment cannot make a round stall or run in a tight loop.
pub(crate) struct GossipRound<'a> {
    pub(crate) number: usize,
    pub(crate) wallclock: u64,
    pub(crate) started_at: Instant,
    pub(crate) stakes: &'a HashMap<Pubkey, u64>,
}

impl<'a> GossipRound<'a> {
    pub(crate) fn new(number: usize, stakes: &'a HashMap<Pubkey, u64>) -> Self {
        Self {
            number,
            wallclock: timestamp(),
            started_at: Instant::now(),
            stakes,
        }
    }

    pub(crate) fn should_generate_pull(&self, period: usize) -> bool {
        self.number.is_multiple_of(period)
    }

    pub(crate) fn sleep_remaining(&self, period: Duration) -> Option<Duration> {
        period.checked_sub(self.started_at.elapsed())
    }
}

/// When each periodic task of the transmit loop last ran.
///
/// Outlives the round it is queried with, which is why it is separate from
/// [`GossipRound`]. Each `due_for_*` both answers and records, so a caller
/// cannot ask and then forget to mark the task as run.
pub(crate) struct GossipSchedule {
    push: Option<Instant>,
    contact_trace: Option<Instant>,
    contact_save: Option<Instant>,
}

impl GossipSchedule {
    /// The push refresh runs on the first round, so its timer starts unset.
    /// Contact tracing and persistence begin their intervals now instead, so
    /// neither fires before the table has had a chance to fill.
    pub(crate) fn new() -> Self {
        let started = Some(Instant::now());
        Self {
            push: None,
            contact_trace: started,
            contact_save: started,
        }
    }

    pub(crate) fn due_for_push(&mut self, round: &GossipRound<'_>, interval: Duration) -> bool {
        Self::claim(&mut self.push, round, interval)
    }

    pub(crate) fn due_for_contact_trace(
        &mut self,
        round: &GossipRound<'_>,
        interval: Duration,
    ) -> bool {
        Self::claim(&mut self.contact_trace, round, interval)
    }

    pub(crate) fn due_for_contact_save(
        &mut self,
        round: &GossipRound<'_>,
        interval: Duration,
    ) -> bool {
        Self::claim(&mut self.contact_save, round, interval)
    }

    fn claim(last: &mut Option<Instant>, round: &GossipRound<'_>, interval: Duration) -> bool {
        let due = last.is_none_or(|last| round.started_at.duration_since(last) > interval);
        if due {
            *last = Some(round.started_at);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_scheduling() {
        let stakes = HashMap::new();
        let round = GossipRound::new(10, &stakes);
        assert!(round.should_generate_pull(5));
        assert!(!round.should_generate_pull(3));
        assert_eq!(round.sleep_remaining(Duration::ZERO), None);
    }

    // Claiming a task marks it, so the same round cannot run it twice.
    #[test]
    fn test_schedule_claims_once_per_interval() {
        let stakes = HashMap::new();
        let round = GossipRound::new(0, &stakes);
        let mut schedule = GossipSchedule::new();
        let interval = Duration::from_secs(3600);
        assert!(schedule.due_for_push(&round, interval));
        assert!(!schedule.due_for_push(&round, interval));

        let later = GossipRound {
            started_at: round.started_at + interval * 2,
            ..GossipRound::new(1, &stakes)
        };
        assert!(schedule.due_for_push(&later, interval));
        assert!(schedule.due_for_contact_trace(&later, interval));
        // Tasks are tracked independently.
        assert!(!schedule.due_for_push(&later, interval));
    }

    // Contact tracing and persistence must not fire on the first round.
    #[test]
    fn test_contact_timers_start_armed() {
        let stakes = HashMap::new();
        let round = GossipRound::new(0, &stakes);
        let mut schedule = GossipSchedule::new();
        let interval = Duration::from_secs(3600);
        assert!(!schedule.due_for_contact_trace(&round, interval));
        assert!(!schedule.due_for_contact_save(&round, interval));
    }
}
