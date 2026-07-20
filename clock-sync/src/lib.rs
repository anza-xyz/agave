//! Byzantine fault-tolerant clock synchronization.
//!
//! Phase 1 (this crate): the Welch-Lynch fine loop with absorption and
//! link-delay calibration, running in shadow mode. Every validator broadcasts
//! a [`protocol::Message::Pulse`] once per [`PULSE_PERIOD`] over an
//! authenticated QUIC datagram channel, estimates every peer's clock offset
//! from pulse arrival times, and steers a virtual clock ([`clock::SyncedClock`])
//! by the stake-weighted fault-tolerant midpoint of those estimates. Nothing
//! consensus-facing reads the virtual clock yet; the service only emits
//! metrics.
//!
//! The coarse loop (`Panic` + recovery via consensus) is Phase 2 and is not
//! implemented here; the wire format reserves a message type for it.
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
///
/// Must be strictly less than [`PULSE_PERIOD`]: round r closes at
/// `next_r + W`, and only then is the round r+1 pulse time known, so the
/// close must happen before that pulse is due even after a maximally
/// negative correction. W = T/2 makes adjacent acceptance windows touch
/// without overlapping.
pub const ACCEPTANCE_WINDOW: Duration = Duration::from_millis(500);

/// S: half-width of the cluster that absorption looks for. A validator that
/// cannot find a quorum of in-window pulses snaps to any cluster of width
/// 2S that carries more than 2/3 of stake.
pub const ABSORPTION_HALF_WIDTH: Duration = Duration::from_millis(50);

/// Sustained inbound datagrams-per-second each peer may send us. The
/// protocol rate is 1 pulse/second; headroom covers retransmits after
/// connection churn and future message types.
pub const CLOCK_SYNC_RATE_LIMIT_PPS: usize = 5;
