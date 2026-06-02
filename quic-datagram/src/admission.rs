//! Admission policy: decides whether the local endpoint will accept
//! a TLS-attested peer pubkey.

use {
    arc_swap::ArcSwap,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

/// Called once per inbound handshake (after the peer cert is parsed) and once
/// per outbound dial-success. A `false` answer closes the connection with the
/// `NOT_ADMITTED` error code.
///
/// Implementations must be cheap - this is on the hot path of every new
/// connection.
pub trait Admission: Send + Sync + 'static {
    fn allow(&self, peer: &Pubkey) -> bool;
}

/// Snapshot of the currently-admitted peer set with their epoch stake.
/// Producers (typically the staked-validators cache) call
/// [`StakedNodesAdmission::swap`] to publish a new generation; consumers
/// see it on the next `allow` call.
///
/// Stake values are available for future use (e.g. weighted eviction).
/// Peers inserted for test-only purposes (e.g. `extra_admit`) carry stake 0.
#[derive(Default)]
pub struct StakedNodesAdmission {
    inner: ArcSwap<HashMap<Pubkey, u64>>,
}

impl StakedNodesAdmission {
    pub fn new(initial: HashMap<Pubkey, u64>) -> Self {
        Self {
            inner: ArcSwap::new(Arc::new(initial)),
        }
    }

    /// Publish a new admit-set. Atomic; no readers block.
    pub fn swap(&self, next: HashMap<Pubkey, u64>) {
        self.inner.store(Arc::new(next));
    }

    /// Number of admitted peers in the current generation.
    pub fn len(&self) -> usize {
        self.inner.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.load().is_empty()
    }
}

impl Admission for StakedNodesAdmission {
    fn allow(&self, peer: &Pubkey) -> bool {
        self.inner.load().contains_key(peer)
    }
}

/// Admit every peer. **Test/bench use only** - gated behind
/// `dev-context-only-utils` so production binaries cannot accidentally use it.
#[cfg(any(test, feature = "dev-context-only-utils"))]
pub struct AllowAll;

#[cfg(any(test, feature = "dev-context-only-utils"))]
impl Admission for AllowAll {
    fn allow(&self, _: &Pubkey) -> bool {
        true
    }
}
