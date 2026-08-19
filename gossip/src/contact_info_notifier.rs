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
    agave_geyser_notifier_interface::contact_info_notifier::ContactInfoSnapshot,
};

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
