//! This module provides [`LeaderUpdater`] trait.
//!
//! Currently, the main purpose of [`LeaderUpdater`] is to abstract over leader
//! updates, hiding the details of how leaders are retrieved and which
//! structures are used.

use {
    async_trait::async_trait,
    std::{fmt, net::SocketAddr},
    thiserror::Error,
};

/// [`LeaderUpdater`] trait abstracts out functionality required for the
/// [`ConnectionWorkersScheduler`](crate::ConnectionWorkersScheduler) to
/// identify next leaders to send transactions to.
#[async_trait]
pub trait LeaderUpdater: Send {
    /// Returns next leaders for the next `lookahead_leaders` starting from
    /// current estimated slot.
    ///
    /// Leaders are returned per [`NUM_CONSECUTIVE_LEADER_SLOTS`] to avoid unnecessary repetition.
    ///
    /// If the current leader estimation is incorrect and transactions are sent to
    /// only one estimated leader, there is a risk of losing all the transactions,
    /// depending on the forwarding policy.
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr>;

    /// Stop [`LeaderUpdater`] and releases all associated resources.
    async fn stop(&mut self);
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
#[async_trait]
impl LeaderUpdater for PinnedLeaderUpdater {
    fn next_leaders(&mut self, _lookahead_leaders: usize) -> Vec<SocketAddr> {
        self.address.clone()
    }

    async fn stop(&mut self) {}
}
