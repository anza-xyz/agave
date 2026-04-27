//! Locks the invariant that the scheduler-bindings handshake rejects exactly the same TPU
//! override addresses that `solana_gossip::contact_info::sanitize_socket` rejects. If gossip
//! ever tightens its socket validation, this test will fail until the handshake is updated to
//! match — otherwise [`crate::scheduler_bindings_server::apply_tpu_override`] would panic.

#![cfg(unix)]

use {
    agave_scheduling_utils::handshake::{AgaveHandshakeError, ClientLogon, server::Server},
    solana_gossip::contact_info::{ContactInfo, Protocol},
    solana_pubkey::Pubkey,
    std::net::{Ipv4Addr, SocketAddr},
};

fn handshake_accepts(addr: Ipv4Addr, port: u16) -> bool {
    let logon = ClientLogon {
        worker_count: 1,
        allocator_size: 64 * 1024 * 1024,
        allocator_handles: 1,
        tpu_to_pack_capacity: 1024,
        progress_tracker_capacity: 256,
        pack_to_worker_capacity: 1024,
        worker_to_pack_capacity: 1024,
        flags: 0,
        tpu_override_addr: addr.octets(),
        tpu_override_port: port,
    };
    !matches!(
        Server::setup_session(logon),
        Err(AgaveHandshakeError::InvalidTpuOverride(_))
    )
}

fn gossip_accepts(addr: SocketAddr) -> bool {
    let mut node = ContactInfo::new_localhost(&Pubkey::new_unique(), 0);
    node.set_tpu(Protocol::QUIC, addr).is_ok()
}

#[test]
fn handshake_matches_gossip_sanitize_socket() {
    // Each case is (addr, port). Port 0 is excluded — the handshake reserves it as the
    // "no override" sentinel rather than a value submitted to gossip.
    let cases: &[(Ipv4Addr, u16)] = &[
        (Ipv4Addr::new(127, 0, 0, 1), 12345),
        (Ipv4Addr::new(192, 168, 1, 1), 8000),
        (Ipv4Addr::new(1, 2, 3, 4), 1),
        (Ipv4Addr::UNSPECIFIED, 12345),
        (Ipv4Addr::new(224, 0, 0, 1), 12345),
        (Ipv4Addr::new(239, 255, 255, 255), 12345),
        (Ipv4Addr::BROADCAST, 12345),
    ];

    for &(addr, port) in cases {
        let socket = SocketAddr::from((addr, port));
        let h = handshake_accepts(addr, port);
        let g = gossip_accepts(socket);
        assert_eq!(
            h, g,
            "handshake/gossip disagree on {socket}: handshake={h}, gossip={g}"
        );
    }
}
