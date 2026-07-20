//! Byzantine fault-tolerant clock synchronization; see the protocol
//! description in <https://github.com/anza-xyz/agave/issues/13981>.
//!
//! Phase 1: the fine loop with absorption and delay calibration, in shadow
//! mode — nothing consensus-facing reads the synchronized clock.
#![cfg(feature = "agave-unstable-api")]

pub mod clock;
pub mod delay;
pub mod launch;
pub mod protocol;
pub mod service;
pub(crate) mod stats;
pub mod welch_lynch;

use std::time::Duration;

/// ALPN protocol identifier for the clock-sync datagram transport.
pub const CLOCK_SYNC_ALPN: &[u8] = b"clocksync-v1";

/// T: how often every validator broadcasts a pulse.
pub const PULSE_PERIOD: Duration = Duration::from_secs(1);

/// W: how far from its expected arrival a pulse may land and still count.
/// Must be strictly less than T: round r closes at `next_r + W`, which must
/// come before the round r+1 pulse even after a maximally negative correction.
pub const ACCEPTANCE_WINDOW: Duration = Duration::from_millis(500);

/// S: half-width of the cluster that absorption looks for.
pub const ABSORPTION_HALF_WIDTH: Duration = Duration::from_millis(50);

/// Sustained inbound datagrams-per-second each peer may send us; the
/// protocol rate is 1 pulse/second.
pub const CLOCK_SYNC_RATE_LIMIT_PPS: usize = 5;
