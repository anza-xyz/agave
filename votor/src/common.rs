use {agave_votor_messages::fraction::Fraction, std::time::Duration};

// Core consensus types and constants
pub(crate) type Stake = u64;

pub(crate) const MAX_NOTAR_FALLBACK_BLOCKS: usize = 7;

pub(crate) const SAFE_TO_NOTAR_MIN_NOTARIZE_ONLY: Fraction = Fraction::from_percentage(40);
pub(crate) const SAFE_TO_NOTAR_MIN_NOTARIZE_FOR_NOTARIZE_OR_SKIP: Fraction =
    Fraction::from_percentage(20);
pub(crate) const SAFE_TO_NOTAR_MIN_NOTARIZE_AND_SKIP: Fraction = Fraction::from_percentage(60);

pub(crate) const SAFE_TO_SKIP_THRESHOLD: Fraction = Fraction::from_percentage(40);

/// Time bound assumed on network transmission delays during periods of synchrony.
pub const DELTA: Duration = Duration::from_millis(250);

/// Base leader handover timeout: Time after parent-ready that a validator would
/// see a leaders first fec set if that leader sent it at the very start of their
/// window.
///
/// With the current 400ms slot duration, this schedules both
/// `TimeoutCrashedLeader(s)` and `Timeout(s)` at 800ms after `ParentReady`.
pub(crate) const DELTA_TIMEOUT: Duration = Duration::from_millis(400);

/// Timeout for standstill detection mechanism.
pub(crate) const DELTA_STANDSTILL: Duration = Duration::from_millis(10_000);
