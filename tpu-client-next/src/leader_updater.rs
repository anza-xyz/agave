//! This module provides [`LeaderUpdater`] trait.
//!
//! Currently, the main purpose of [`LeaderUpdater`] is to abstract over leader
//! updates, hiding the details of how leaders are retrieved and which
//! structures are used.
use {
    solana_clock::Slot,
    std::{fmt, net::SocketAddr},
    thiserror::Error,
};

/// [`LeaderUpdater`] trait abstracts out functionality required for the
/// [`ConnectionWorkersScheduler`](crate::ConnectionWorkersScheduler) to
/// identify next leaders to send transactions to.
pub trait LeaderUpdater: Send {
    /// Appends TPU addresses for upcoming leader windows to `leaders`.
    ///
    /// `lookahead_leaders` controls how many scheduled leader windows are inspected but
    /// implementation may append additional addresses on the leader-window boundary.
    /// Implementations may return duplicate addresses. The scheduler is responsible for deriving
    /// unique send and connect target sets from these ordered candidates.
    ///
    /// If the current leader estimation is incorrect and transactions are sent to
    /// only one estimated leader, there is a risk of losing all the transactions,
    /// depending on the forwarding policy.
    ///
    /// Returns the current slot estimate, or `None` if the provider cannot supply one.
    /// The estimate's window-end timestamp may independently be unknown.
    fn next_leaders(
        &mut self,
        lookahead_leaders: usize,
        leaders: &mut Vec<SocketAddr>,
    ) -> Option<SlotEstimate>;
}

/// Estimated current slot and the end timestamp of its leader window.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SlotEstimate {
    pub slot: Slot,
    /// Estimated window end in milliseconds since the Unix epoch; `None` when unknown.
    pub leader_window_end_ms: Option<u64>,
}

/// Error type for [`LeaderUpdater`].
#[derive(Error, PartialEq)]
pub struct LeaderUpdaterError;

impl fmt::Display for LeaderUpdaterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Leader updater encountered an error")
    }
}

impl fmt::Debug for LeaderUpdaterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LeaderUpdaterError")
    }
}

#[cfg(feature = "dev-context-only-utils")]
pub fn create_pinned_leader_updater(address: SocketAddr) -> Box<dyn LeaderUpdater> {
    Box::new(PinnedLeaderUpdater {
        address: vec![address],
    })
}

/// `PinnedLeaderUpdater` is an implementation of [`LeaderUpdater`] that always
/// returns a fixed, "pinned" leader address.
#[cfg(feature = "dev-context-only-utils")]
struct PinnedLeaderUpdater {
    pub address: Vec<SocketAddr>,
}

#[cfg(feature = "dev-context-only-utils")]
impl LeaderUpdater for PinnedLeaderUpdater {
    fn next_leaders(
        &mut self,
        _lookahead_leaders: usize,
        leaders: &mut Vec<SocketAddr>,
    ) -> Option<SlotEstimate> {
        leaders.extend_from_slice(&self.address);
        None
    }
}
