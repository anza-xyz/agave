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
/// Wall-clock time is sampled once for CRDS timestamps. Scheduling and sleep
/// calculations use `Instant`, so a system clock adjustment cannot make a
/// round stall or run in a tight loop.
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

    pub(crate) fn is_due(&self, last: Option<Instant>, interval: Duration) -> bool {
        last.is_none_or(|last| self.started_at.duration_since(last) > interval)
    }

    pub(crate) fn should_generate_pull(&self, period: usize) -> bool {
        self.number.is_multiple_of(period)
    }

    pub(crate) fn sleep_remaining(&self, period: Duration) -> Option<Duration> {
        period.checked_sub(self.started_at.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::time::Duration};

    #[test]
    fn test_round_scheduling() {
        let stakes = HashMap::new();
        let round = GossipRound::new(10, &stakes);
        assert!(round.should_generate_pull(5));
        assert!(!round.should_generate_pull(3));
        assert!(round.is_due(None, Duration::from_secs(1)));
        assert!(!round.is_due(Some(round.started_at), Duration::from_secs(1)));
        assert!(round.sleep_remaining(Duration::from_secs(1)).is_some());
    }
}
