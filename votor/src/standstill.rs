use {
    solana_clock::Slot,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    std::{
        num::NonZero,
        sync::atomic::{AtomicU32, AtomicU64, Ordering},
        time::Duration,
    },
};

/// Timeout for standstill detection mechanism.
pub(crate) const DELTA_STANDSTILL: Duration = Duration::from_millis(10_000);

/// Maximum standstill timeout extension, capped at 1 hour.
pub const MAX_STANDSTILL_TIMEOUT: Duration = Duration::from_secs(3600);

/// Growth factor applied per elapsed leader window during standstill.
const BACKOFF_PER_LEADER_WINDOW: f64 = 1.05;

/// Sentinel value representing "no active standstill" in [`StandstillSignal`].
const NO_STANDSTILL: u64 = u64::MAX;

/// Shared signal for standstill state under unknown network DELTA.
///
/// Holds the highest finalized slot at the moment standstill was first detected
/// (or no active standstill), plus the backoff reached so far. Read by every
/// subsystem that needs to scale its network-DELTA-derived timeouts.
///
/// The backoff is published by votor via [`Self::record_timeout_slot`] as it arms
/// each skip timeout, and consumed by other subsystems via [`Self::scale`]. Votor's
/// timer is the only component that reliably advances while the cluster is stuck,
/// so it is the authority on how long the standstill has lasted; everyone else
/// inherits that curve instead of deriving one from their own local progress.
#[derive(Debug)]
pub struct StandstillSignal {
    /// Highest finalized slot when standstill began, or [`NO_STANDSTILL`].
    standstill_slot: AtomicU64,
    /// Leader windows elapsed since standstill began, as last published by votor.
    /// Zero when no standstill is active.
    backoff_windows: AtomicU32,
}

impl Default for StandstillSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl StandstillSignal {
    pub fn new() -> Self {
        Self {
            standstill_slot: AtomicU64::new(NO_STANDSTILL),
            backoff_windows: AtomicU32::new(0),
        }
    }

    pub fn get(&self) -> Option<Slot> {
        match self.standstill_slot.load(Ordering::Relaxed) {
            NO_STANDSTILL => None,
            slot => Some(slot),
        }
    }

    pub fn set(&self, slot: Slot) {
        self.standstill_slot.store(slot, Ordering::Relaxed);
        self.backoff_windows.store(0, Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.standstill_slot.store(NO_STANDSTILL, Ordering::Relaxed);
        self.backoff_windows.store(0, Ordering::Relaxed);
    }

    /// Publishes the slot votor is arming a timeout for, advancing the shared
    /// backoff. No-op outside standstill, or for a slot that does not advance it.
    pub fn record_timeout_slot(&self, slot: Slot) {
        let Some(standstill_slot) = self.get() else {
            return;
        };
        let windows = leader_windows_between(standstill_slot, slot);
        self.backoff_windows.fetch_max(windows, Ordering::Relaxed);
    }

    /// Current backoff multiplier, `1.0` outside standstill.
    pub fn multiplier(&self) -> f64 {
        if self.get().is_none() {
            return 1.0;
        }
        multiplier_for_windows(self.backoff_windows.load(Ordering::Relaxed))
    }

    /// Scales `base` by the current backoff, capped at [`MAX_STANDSTILL_TIMEOUT`].
    ///
    /// This is the entry point for subsystems other than votor: it reuses the
    /// backoff votor has already reached rather than recomputing one from local
    /// state that may not advance while the cluster is stuck.
    pub fn scale(&self, base: Duration) -> Duration {
        apply_multiplier(base, self.multiplier())
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
    apply_multiplier(base, calculate_timeout_multiplier(slot, standstill_slot))
}

/// Clamps `base * multiplier` into `[base, MAX_STANDSTILL_TIMEOUT]`.
fn apply_multiplier(base: Duration, multiplier: f64) -> Duration {
    if !multiplier.is_finite() {
        return MAX_STANDSTILL_TIMEOUT;
    }
    let scaled_secs = base.as_secs_f64() * multiplier;
    let clamped_secs = scaled_secs.clamp(base.as_secs_f64(), MAX_STANDSTILL_TIMEOUT.as_secs_f64());
    Duration::try_from_secs_f64(clamped_secs).unwrap_or(MAX_STANDSTILL_TIMEOUT)
}

/// Leader windows elapsed from `standstill_slot` to `slot`, saturating at
/// [`u32::MAX`] and returning zero if `slot` precedes the standstill.
fn leader_windows_between(standstill_slot: Slot, slot: Slot) -> u32 {
    let slots_since_standstill = slot.saturating_sub(standstill_slot);
    let windows = slots_since_standstill
        / NonZero::<Slot>::try_from(NUM_CONSECUTIVE_LEADER_SLOTS)
            .expect("less than 2^64 consecutive leader slots");
    u32::try_from(windows).unwrap_or(u32::MAX)
}

/// Multiplier for a given number of elapsed leader windows.
///
/// Returns `1.05^n`, or [`f64::INFINITY`] once `n` exceeds what `powi` accepts —
/// callers clamp to [`MAX_STANDSTILL_TIMEOUT`] either way.
fn multiplier_for_windows(windows: u32) -> f64 {
    match i32::try_from(windows) {
        Ok(windows) => BACKOFF_PER_LEADER_WINDOW.powi(windows),
        Err(_) => f64::INFINITY,
    }
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
            multiplier_for_windows(leader_windows_between(standstill_slot, slot))
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

        // A slot before the standstill does not shrink the timeout.
        assert_eq!(calculate_timeout_multiplier(0, Some(100)), 1.0);
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

    #[test]
    fn test_signal_backoff_tracks_recorded_timeout_slots() {
        let base = DELTA_TIMEOUT;
        let s = StandstillSignal::new();

        // Outside standstill nothing scales, and recording is inert.
        s.record_timeout_slot(1000);
        assert_eq!(s.multiplier(), 1.0);
        assert_eq!(s.scale(base), base);

        // Standstill begins; no windows elapsed yet.
        s.set(100);
        assert_eq!(s.multiplier(), 1.0);

        // Votor arms a timeout two leader windows out.
        s.record_timeout_slot(108);
        assert!((s.multiplier() - 1.05_f64.powi(2)).abs() < 0.001);
        assert_eq!(
            s.scale(base),
            scale_standstill_timeout(base, 108, Some(100))
        );

        // The backoff only grows: a stale or lower slot does not walk it back.
        s.record_timeout_slot(104);
        assert!((s.multiplier() - 1.05_f64.powi(2)).abs() < 0.001);

        // Recovery resets the backoff.
        s.clear();
        assert_eq!(s.multiplier(), 1.0);
        assert_eq!(s.scale(base), base);

        // A fresh standstill starts from scratch, not from the previous backoff.
        s.set(200);
        assert_eq!(s.multiplier(), 1.0);
    }

    #[test]
    fn test_signal_scale_caps_at_max() {
        let s = StandstillSignal::new();
        s.set(0);
        // 400 leader windows: 1.05^400 is far above the 1h cap.
        s.record_timeout_slot(4 * 400);
        assert_eq!(s.scale(DELTA_TIMEOUT), MAX_STANDSTILL_TIMEOUT);
        // Absurd slot distances saturate rather than overflow.
        s.record_timeout_slot(u64::MAX);
        assert_eq!(s.scale(DELTA_TIMEOUT), MAX_STANDSTILL_TIMEOUT);
    }
}
