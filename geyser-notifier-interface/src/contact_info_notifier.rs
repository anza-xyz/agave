//! Types carried on the gossip → Geyser contact info channel.
//!
//! Gossip converts each accepted CRDS contact info update into an owned
//! [`ContactInfoSnapshot`] and pushes a [`ContactInfoEvent`] through a
//! bounded channel (see `solana_gossip::contact_info_notifier` for the
//! producer side and its non-blocking delivery semantics). All filtering,
//! deduplication, threading, and plugin dispatch happens on the consumer
//! side, in the Geyser plugin manager.

use {solana_pubkey::Pubkey, std::net::SocketAddr};

/// An owned snapshot of a validator's gossip-advertised contact info.
///
/// Produced by gossip the moment a new or updated `ContactInfo` is accepted
/// into the CRDS table (i.e. after the `overrides()` dedup check has passed).
/// Every field is a plain value with no borrows or heap allocations back
/// into gossip internals, so the snapshot is `Copy` and can be freely sent
/// across threads and buffered in a queue without per-event allocation.
#[derive(Clone, Copy, Debug)]
pub struct ContactInfoSnapshot {
    pub pubkey: Pubkey,
    pub wallclock: u64,
    pub outset: u64,
    pub shred_version: u16,
    /// Major component of the software version (e.g. `1` in `1.18.25`).
    pub version_major: u16,
    /// Minor component of the software version.
    pub version_minor: u16,
    /// Patch component of the software version.
    pub version_patch: u16,
    /// First four bytes of the build commit hash (`0` when unset).
    pub version_commit: u32,
    /// Active feature set advertised by the validator.
    pub version_feature_set: u32,
    /// Client identifier, as encoded by `solana_version::ClientId` to `u16`.
    pub version_client_id: u16,
    pub gossip: Option<SocketAddr>,
    pub tpu_quic: Option<SocketAddr>,
    pub tpu_forwards_quic: Option<SocketAddr>,
    pub tpu_vote_udp: Option<SocketAddr>,
    pub tpu_vote_quic: Option<SocketAddr>,
    pub tvu_udp: Option<SocketAddr>,
    pub tvu_quic: Option<SocketAddr>,
    pub serve_repair_udp: Option<SocketAddr>,
    pub serve_repair_quic: Option<SocketAddr>,
    pub rpc: Option<SocketAddr>,
    pub rpc_pubsub: Option<SocketAddr>,
    pub alpenglow: Option<SocketAddr>,
}

/// Lifecycle events emitted by gossip for contact info entries. `Updated`
/// covers both first-seen and semantic-change cases (CRDS doesn't
/// distinguish them at the call site, and consumers usually don't need to
/// either). `Removed` fires when CRDS evicts an entry — either via
/// timeout-based purging (a validator stopped gossiping) or size-based
/// trimming — so that consumers can invalidate cached endpoints rather
/// than letting them age out via their own staleness heuristic.
///
/// `Removed` carries only the identity pubkey because that's all CRDS
/// knows at eviction time; the full state at the time of removal was
/// whatever the most recent `Updated` event delivered.
///
/// The enum is intentionally sized to the largest variant
/// (`ContactInfoSnapshot`, ~250 bytes). Boxing `Updated` would add a
/// heap allocation per emit on the gossip hot path, which is the
/// frequent case; `Removed` is rare by comparison (CRDS evictions
/// happen at the rate of cluster churn, not per-rebroadcast), so the
/// extra ~250 bytes per `Removed` event is a non-issue.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug)]
pub enum ContactInfoEvent {
    Updated(ContactInfoSnapshot),
    Removed(Pubkey),
}

/// Sender half of the contact info channel. Owned by the CRDS table when
/// a consumer is attached.
pub type ContactInfoSender = crossbeam_channel::Sender<ContactInfoEvent>;

/// Receiver half of the contact info channel. Owned by the consumer
/// (typically a dispatch thread inside the Geyser plugin manager).
pub type ContactInfoReceiver = crossbeam_channel::Receiver<ContactInfoEvent>;
