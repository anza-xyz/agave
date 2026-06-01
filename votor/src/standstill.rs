use {
    solana_clock::Slot,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    std::{
        num::NonZero,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    },
};

/// Timeout for standstill detection mechanism.
pub(crate) const DELTA_STANDSTILL: Duration = Duration::from_millis(10_000);

/// Maximum standstill timeout extension, capped at 1 hour.
pub const MAX_STANDSTILL_TIMEOUT: Duration = Duration::from_secs(3600);

/// Sentinel value representing "no active standstill" in [`StandstillSignal`].
const NO_STANDSTILL: u64 = u64::MAX;

/// Shared signal for standstill state under unknown network DELTA.
///
/// Holds either no active standstill, or the highest finalized slot at the
/// moment standstill was first detected. Read by every subsystem that needs to
/// scale its network-DELTA-derived timeouts (votor timer manager, repair).
#[derive(Debug)]
pub struct StandstillSignal {
    inner: AtomicU64,
}

impl Default for StandstillSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl StandstillSignal {
    pub fn new() -> Self {
        Self {
            inner: AtomicU64::new(NO_STANDSTILL),
        }
    }

    pub fn get(&self) -> Option<Slot> {
        match self.inner.load(Ordering::Relaxed) {
            NO_STANDSTILL => None,
            slot => Some(slot),
        }
    }

    pub fn set(&self, slot: Slot) {
        self.inner.store(slot, Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.inner.store(NO_STANDSTILL, Ordering::Relaxed);
    }
}

/// Scale a base timeout by the correct standstill multiplier and cap at
/// [`MAX_STANDSTILL_TIMEOUT`].
///
/// Gracefully handles overflow by clamping to [`MAX_STANDSTILL_TIMEOUT`],
/// and underflow (i.e. `slot` before standstill) by returning `base`.
pub fn scale_standstill_timeout(
    base: Duration,
    slot: Slot,
    standstill_slot: Option<Slot>,
) -> Duration {
    let multiplier = calculate_timeout_multiplier(slot, standstill_slot);
    if !multiplier.is_finite() {
        return MAX_STANDSTILL_TIMEOUT;
    }
    let scaled_secs = base.as_secs_f64() * multiplier;
    let clamped_secs = scaled_secs.clamp(base.as_secs_f64(), MAX_STANDSTILL_TIMEOUT.as_secs_f64());
    Duration::try_from_secs_f64(clamped_secs).unwrap_or(MAX_STANDSTILL_TIMEOUT)
}

/// Multiplier applied to network-DELTA-derived timeouts under unknown DELTA.
///
/// Returns 1.0 outside standstill. During standstill, returns `1.05^n` where
/// `n` is the number of leader windows elapsed since standstill began. Used
/// to discover the actual network DELTA by exponentially growing every retry
/// timeout — see Alpenglow partial-synchrony recovery.
fn calculate_timeout_multiplier(slot: Slot, standstill_slot: Option<Slot>) -> f64 {
    match standstill_slot {
        None => 1.0,
        Some(standstill_slot) => {
            if slot < standstill_slot {
                return 1.0;
            }
            if slot.saturating_sub(standstill_slot) > i32::MAX as u64 {
                return f64::INFINITY;
            }
            let slots_since_standstill = slot.saturating_sub(standstill_slot);
            let leader_windows = slots_since_standstill
                / NonZero::<Slot>::try_from(NUM_CONSECUTIVE_LEADER_SLOTS)
                    .expect("less than 2^64 consecutive leader slots");
            1.05_f64.powi(i32::try_from(leader_windows).expect("handled above"))
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::common::DELTA_TIMEOUT};

    #[test]
    fn test_calculate_timeout_multiplier() {
        // No standstill - multiplier is 1.0.
        assert_eq!(calculate_timeout_multiplier(100, None), 1.0);

        // At standstill_slot itself - 0 leader windows elapsed.
        assert_eq!(calculate_timeout_multiplier(0, Some(0)), 1.0);

        // 4 slots = 1 leader window = 1.05^1.
        let m = calculate_timeout_multiplier(4, Some(0));
        assert!((m - 1.05).abs() < 0.001);

        // 8 slots = 2 leader windows = 1.05^2.
        let m = calculate_timeout_multiplier(8, Some(0));
        assert!((m - 1.1025).abs() < 0.001);

        // 40 slots = 10 leader windows = 1.05^10.
        let m = calculate_timeout_multiplier(40, Some(0));
        assert!((m - 1.05_f64.powi(10)).abs() < 0.001);

        // 8 slots after standstill_slot=20.
        let m = calculate_timeout_multiplier(28, Some(20));
        assert!((m - 1.1025).abs() < 0.001);
    }

    #[test]
    fn test_standstill_signal() {
        let s = StandstillSignal::new();
        assert_eq!(s.get(), None);
        s.set(42);
        assert_eq!(s.get(), Some(42));
        s.set(100);
        assert_eq!(s.get(), Some(100));
        s.clear();
        assert_eq!(s.get(), None);
    }

    #[test]
    fn test_scale_standstill_timeout_caps() {
        let base = DELTA_TIMEOUT;
        // Without standstill, no scaling.
        assert_eq!(scale_standstill_timeout(base, 100, None), base);
        // With absurdly large window count, capped at MAX_STANDSTILL_TIMEOUT.
        let capped = scale_standstill_timeout(base, 4 * 400, Some(0));
        assert_eq!(capped, MAX_STANDSTILL_TIMEOUT);
        // Instead of overflowing, capped at MAX_STANDSTILL_TIMEOUT.
        let capped = scale_standstill_timeout(base, u64::MAX, Some(0));
        assert_eq!(capped, MAX_STANDSTILL_TIMEOUT);
        // This should never happen, but timeout does not drop below base.
        assert_eq!(scale_standstill_timeout(base, 0, Some(100)), base);
    }
}
