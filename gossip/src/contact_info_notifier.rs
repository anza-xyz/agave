//! Lightweight channel wiring that lets a consumer (typically the Geyser
//! plugin manager) observe CRDS contact info updates as they are accepted
//! into the table.
//!
//! The channel is optional: when no consumer is attached, gossip does no
//! work at all on the hot path. When a consumer is attached, each accepted
//! contact info update is converted into an owned `ContactInfoSnapshot`
//! and pushed through a bounded channel via `try_send`. If the channel is
//! full the event is dropped — contact info rebroadcasts through gossip on
//! a multi-second cadence, so consumers self-heal on the next republish.
//!
//! All filtering, deduplication, threading, and plugin dispatch happens on
//! the consumer side (see `agave-geyser-plugin-manager`). Gossip's only
//! responsibility is to emit a snapshot per accepted CRDS insert.

use {
    crate::contact_info::{ContactInfo, Protocol},
    solana_pubkey::Pubkey,
    std::net::SocketAddr,
};

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

impl From<&ContactInfo> for ContactInfoSnapshot {
    fn from(info: &ContactInfo) -> Self {
        let v = info.version();
        let client_id_u16 = u16::try_from(v.client().clone()).unwrap_or(u16::MAX);
        Self {
            pubkey: *info.pubkey(),
            wallclock: info.wallclock(),
            outset: info.outset(),
            shred_version: info.shred_version(),
            version_major: v.major(),
            version_minor: v.minor(),
            version_patch: v.patch(),
            version_commit: v.commit(),
            version_feature_set: v.feature_set(),
            version_client_id: client_id_u16,
            gossip: info.gossip(),
            tpu_quic: info.tpu(Protocol::QUIC),
            tpu_forwards_quic: info.tpu_forwards(Protocol::QUIC),
            tpu_vote_udp: info.tpu_vote(Protocol::UDP),
            tpu_vote_quic: info.tpu_vote(Protocol::QUIC),
            tvu_udp: info.tvu(Protocol::UDP),
            tvu_quic: info.tvu(Protocol::QUIC),
            serve_repair_udp: info.serve_repair(Protocol::UDP),
            serve_repair_quic: info.serve_repair(Protocol::QUIC),
            rpc: info.rpc(),
            rpc_pubsub: info.rpc_pubsub(),
            alpenglow: info.alpenglow(),
        }
    }
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_pubkey::Pubkey,
        std::net::{IpAddr, Ipv4Addr, SocketAddr},
    };

    #[test]
    fn snapshot_from_contact_info_preserves_pubkey_and_versions() {
        let pk = Pubkey::new_unique();
        let info = ContactInfo::new(pk, /*wallclock:*/ 42, /*shred_version:*/ 7);
        let snap = ContactInfoSnapshot::from(&info);

        assert_eq!(snap.pubkey, pk);
        assert_eq!(snap.wallclock, 42);
        assert_eq!(snap.shred_version, 7);
        // outset is set by ContactInfo::new based on wall clock; just sanity check it's > 0
        assert!(snap.outset > 0);
        // version-component getters are exercised here, satisfying the public-API surface
        let v = info.version();
        assert_eq!(snap.version_major, v.major());
        assert_eq!(snap.version_minor, v.minor());
        assert_eq!(snap.version_patch, v.patch());
        assert_eq!(snap.version_commit, v.commit());
        assert_eq!(snap.version_feature_set, v.feature_set());
        // No sockets advertised yet — all should be None
        assert!(snap.gossip.is_none());
        assert!(snap.tpu_quic.is_none());
        assert!(snap.tpu_forwards_quic.is_none());
        assert!(snap.tpu_vote_udp.is_none());
        assert!(snap.tpu_vote_quic.is_none());
        assert!(snap.tvu_udp.is_none());
        assert!(snap.tvu_quic.is_none());
        assert!(snap.serve_repair_udp.is_none());
        assert!(snap.serve_repair_quic.is_none());
        assert!(snap.rpc.is_none());
        assert!(snap.rpc_pubsub.is_none());
        assert!(snap.alpenglow.is_none());
    }

    #[test]
    fn snapshot_from_contact_info_resolves_advertised_sockets() {
        let mut info = ContactInfo::new(
            Pubkey::new_unique(),
            /*wallclock:*/ 0,
            /*shred_version:*/ 1,
        );
        let addr = |port: u16| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        info.set_gossip(addr(8000)).unwrap();
        info.set_rpc(addr(8899)).unwrap();
        info.set_rpc_pubsub(addr(8900)).unwrap();
        info.set_tpu(Protocol::QUIC, addr(8004)).unwrap();
        info.set_tpu_forwards(Protocol::QUIC, addr(8005)).unwrap();
        info.set_tpu_vote(Protocol::UDP, addr(8006)).unwrap();
        info.set_tpu_vote(Protocol::QUIC, addr(8007)).unwrap();
        info.set_tvu(Protocol::UDP, addr(8008)).unwrap();
        info.set_tvu(Protocol::QUIC, addr(8009)).unwrap();
        info.set_serve_repair(Protocol::UDP, addr(8010)).unwrap();
        info.set_serve_repair(Protocol::QUIC, addr(8011)).unwrap();
        info.set_alpenglow(addr(8012)).unwrap();

        let snap = ContactInfoSnapshot::from(&info);
        assert_eq!(snap.gossip, Some(addr(8000)));
        assert_eq!(snap.rpc, Some(addr(8899)));
        assert_eq!(snap.rpc_pubsub, Some(addr(8900)));
        assert_eq!(snap.tpu_quic, Some(addr(8004)));
        assert_eq!(snap.tpu_forwards_quic, Some(addr(8005)));
        assert_eq!(snap.tpu_vote_udp, Some(addr(8006)));
        assert_eq!(snap.tpu_vote_quic, Some(addr(8007)));
        assert_eq!(snap.tvu_udp, Some(addr(8008)));
        assert_eq!(snap.tvu_quic, Some(addr(8009)));
        assert_eq!(snap.serve_repair_udp, Some(addr(8010)));
        assert_eq!(snap.serve_repair_quic, Some(addr(8011)));
        assert_eq!(snap.alpenglow, Some(addr(8012)));
    }
}
