#![allow(clippy::arithmetic_side_effects)]

#[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
use std::sync::OnceLock;
use std::time::Instant as StdInstant;

// ── Public API ────────────────────────────────────────────────────────────

/// A high-resolution timer matching the `std::time::Instant` interface.
///
/// When the `rdtsc` feature is enabled, the target is x86/x86_64, and the
/// CPU advertises an invariant TSC, `Instant::now()` reads the hardware
/// counter directly via `RDTSC`.  In all other cases it falls back to
/// `std::time::Instant`, which has exactly the same semantics as the
/// original `measure_us!` macro before this crate existed.
///
/// Call [`Instant::memoize`] once at process startup to pre-calibrate. Otherwise,
/// calibration is done lazily on the first [`elapsed_us`](Instant::elapsed_us)
/// call that needs it.
pub struct Instant {
    inner: Inner,
}

impl Instant {
    /// Capture the current time.
    #[inline(always)]
    pub fn now() -> Self {
        Self {
            inner: Inner::now(),
        }
    }

    /// Elapsed microseconds since [`Instant::now`] was called.
    #[inline(always)]
    pub fn elapsed_us(&self) -> u64 {
        self.inner.elapsed_us()
    }

    /// Elapsed nanoseconds since [`Instant::now`] was called.
    #[inline(always)]
    pub fn elapsed_ns(&self) -> u64 {
        self.inner.elapsed_ns()
    }

    /// Calibrate the TSC and enable the RDTSC fast path.
    ///
    /// Applications sensitive to timing performance should call this once at
    /// process startup to pre-calibrate and avoid lazy init. Without this call,
    /// calibration is done lazily on first use (in the first
    /// [`elapsed_us`](Instant::elapsed_us) that uses RDTSC).
    ///
    /// On non-x86 targets, or when the `rdtsc` feature is disabled, this is a
    /// no-op.
    pub fn memoize() {
        #[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            let tps = *TICKS_PER_SECOND.get_or_init(calibrate);
            MUL_US.get_or_init(|| mul_from_tps(1_000_000, tps));
            MUL_NS.get_or_init(|| mul_from_tps(1_000_000_000, tps));
        }
    }
}

// ── Internal enum ─────────────────────────────────────────────────────────

enum Inner {
    #[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
    Rdtsc(u64),
    StdInstant(StdInstant),
}

impl Inner {
    #[inline(always)]
    fn now() -> Self {
        #[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
        if check_cpu_supports_invariant_tsc() {
            return Self::Rdtsc(rdtsc());
        }
        Self::StdInstant(StdInstant::now())
    }

    #[inline(always)]
    fn elapsed_us(&self) -> u64 {
        match self {
            #[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
            Self::Rdtsc(start) => {
                let ticks = rdtsc().wrapping_sub(*start);
                ((ticks as u128 * mul_us() as u128) >> SHIFT) as u64
            }
            Self::StdInstant(start) => start.elapsed().as_micros() as u64,
        }
    }

    #[inline(always)]
    fn elapsed_ns(&self) -> u64 {
        match self {
            #[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
            Self::Rdtsc(start) => {
                let ticks = rdtsc().wrapping_sub(*start);
                ((ticks as u128 * mul_ns() as u128) >> SHIFT) as u64
            }
            Self::StdInstant(start) => start.elapsed().as_nanos() as u64,
        }
    }
}

// ── Calibration ───────────────────────────────────────────────────────────
//
// Lazily initialized via OnceLock: either by Instant::memoize() at startup, or
// on first mul_us() call when an RDTSC-based Instant needs the value.

#[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
static TICKS_PER_SECOND: OnceLock<u64> = OnceLock::new();

#[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
static MUL_US: OnceLock<u64> = OnceLock::new();

#[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
static MUL_NS: OnceLock<u64> = OnceLock::new();

/// Returns `true` when `/proc/cpuinfo` lists both `constant_tsc` and
/// `nonstop_tsc`, indicating an invariant TSC suitable for wall-clock use.
/// Always `false` on non-x86 targets.
pub fn check_cpu_supports_invariant_tsc() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        static SUPPORTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTS.get_or_init(|| {
            std::fs::read_to_string("/proc/cpuinfo")
                .map(|s| s.contains("constant_tsc") && s.contains("nonstop_tsc"))
                .unwrap_or(false)
        })
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    false
}

// ── x86/x86_64 internals ─────────────────────────────────────────────────

#[cfg(all(feature = "rdtsc", target_arch = "x86_64"))]
#[inline(always)]
fn rdtsc() -> u64 {
    unsafe {
        core::arch::x86_64::_mm_lfence();
        core::arch::x86_64::_rdtsc()
    }
}

#[cfg(all(feature = "rdtsc", target_arch = "x86"))]
#[inline(always)]
fn rdtsc() -> u64 {
    unsafe {
        core::arch::x86::_mm_lfence();
        core::arch::x86::_rdtsc()
    }
}

#[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
const SHIFT: u32 = 32;

/// Precomputed multiplier for converting ticks to microseconds via shift.
/// Lazily derives from ticks_per_second on first call if [`Instant::memoize`]
/// was not invoked at startup.
#[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
fn mul_us() -> u64 {
    *MUL_US.get_or_init(|| {
        let tps = *TICKS_PER_SECOND.get_or_init(calibrate);
        mul_from_tps(1_000_000, tps)
    })
}

/// Precomputed multiplier for converting ticks to nanoseconds via shift.
#[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
fn mul_ns() -> u64 {
    *MUL_NS.get_or_init(|| {
        let tps = *TICKS_PER_SECOND.get_or_init(calibrate);
        mul_from_tps(1_000_000_000, tps)
    })
}

/// `(units_per_second << SHIFT) / ticks_per_second`
#[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
fn mul_from_tps(units_per_second: u128, ticks_per_second: u64) -> u64 {
    ((units_per_second << SHIFT) / ticks_per_second as u128) as u64
}

/// 1 s warmup + 1 s measurement → ticks per second.
#[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
fn calibrate() -> u64 {
    use std::time::Duration;

    let warmup = Duration::from_millis(1000);
    let measure = Duration::from_millis(1000);

    let t0 = StdInstant::now();
    while t0.elapsed() < warmup {}

    let t1 = StdInstant::now();
    let tsc0 = rdtsc();
    while t1.elapsed() < measure {}
    let tsc1 = rdtsc();
    let elapsed_ns = t1.elapsed().as_nanos() as u128;

    ((tsc1 - tsc0) as u128 * 1_000_000_000 / elapsed_ns.max(1)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instant_accuracy() {
        let instant = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let us = instant.elapsed_us();
        assert!(
            (9_000..=30_000).contains(&us),
            "unexpected elapsed_us: {us}"
        );
    }

    #[cfg(all(feature = "rdtsc", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn test_rdtsc_accuracy() {
        Instant::memoize();
        for i in 0..5 {
            let rdtsc_instant = Inner::Rdtsc(rdtsc());
            let std_instant = Inner::StdInstant(StdInstant::now());
            std::thread::sleep(std::time::Duration::from_millis(10));
            let rdtsc_us = rdtsc_instant.elapsed_us();
            let instant_us = std_instant.elapsed_us();
            let pct_diff =
                ((rdtsc_us as f64 - instant_us as f64) / instant_us as f64 * 100.0) as i64;
            println!(
                "  [{i}] Instant: {instant_us} µs  |  rdtsc: {rdtsc_us} µs  |  diff: {pct_diff}%"
            );
            assert!(
                pct_diff.unsigned_abs() < 5,
                "rdtsc calibration off by {pct_diff}%"
            );
        }
    }
}
