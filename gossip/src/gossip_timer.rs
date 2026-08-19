use std::time::{Duration, Instant};

/// A deadline that rearms itself each time it is claimed.
pub(crate) struct Periodic {
    deadline: Instant,
    period: Duration,
}

impl Periodic {
    /// Due immediately, then every `period`.
    pub(crate) fn due_now(now: Instant, period: Duration) -> Self {
        Self {
            deadline: now,
            period,
        }
    }

    /// Due one `period` from now.
    pub(crate) fn due_after(now: Instant, period: Duration) -> Self {
        Self {
            deadline: now + period,
            period,
        }
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn claim(&mut self, now: Instant) -> bool {
        if now < self.deadline {
            return false;
        }
        self.deadline = now + self.period;
        true
    }
}
