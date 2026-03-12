use std::time::{Duration, Instant};

/// A timer that can be started and stopped.
///
/// Will accumulate a total duration in a running state that can be then read out.
#[derive(Default, Clone)]
pub(crate) struct Stopwatch {
    start_time: Option<Instant>,
    accumulated_duration: Duration,
}

impl Stopwatch {
    pub(crate) fn new_running() -> Self {
        let mut this = Self::default();
        this.resume();
        this
    }

    pub(crate) fn resume(&mut self) -> &mut Self {
        match self.start_time {
            Some(_) => {}
            None => {
                self.start_time = Some(Instant::now());
            }
        }
        self
    }

    pub(crate) fn stop(&mut self) -> &mut Self {
        let Some(previous_continue_time) = self.start_time.take() else {
            return self;
        };
        self.accumulated_duration = self
            .accumulated_duration
            .saturating_add(previous_continue_time.elapsed());
        self
    }

    /// Reset the stopwatch.
    ///
    /// Stopwatch will remain in the state it was prior to the the call. If it was running, it will
    /// remain running as if it was resumed from 0 at the time this function returns.
    pub(crate) fn reset(&mut self) -> &mut Self {
        self.take_us();
        self
    }

    /// Same as [`Self::read_us`] followed by [`Self::reset`].
    pub(crate) fn take_us(&mut self) -> u64 {
        let result = self.read_us();
        self.accumulated_duration = Duration::ZERO;
        result
    }

    /// Read the currently accumulated duration in the running state.
    pub(crate) fn read_us(&mut self) -> u64 {
        if let Some(running_since) = &mut self.start_time {
            let now = Instant::now();
            self.accumulated_duration = self
                .accumulated_duration
                .saturating_add(now.duration_since(*running_since));
            *running_since = now;
        }
        self.accumulated_duration.as_micros() as u64
    }
}
