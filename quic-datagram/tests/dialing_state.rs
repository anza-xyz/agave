//! `ConnectionTableEntry::Dialing` placeholder semantics.
//!
//! Covers the lex-lower dialer cleaning up its placeholder on dial
//! failure so a subsequent egress to the same pubkey can retry
//! (`endpoint.connect` timeout, server identity mismatch).

#[path = "common.rs"]
mod common;

use {
    bytes::Bytes,
    common::{
        drain_matching, keypair_below, make_runtime, send_until_received, spawn_node,
        spawn_node_with,
    },
    solana_keypair::Keypair,
    solana_net_utils::sockets::bind_to_localhost_unique,
    solana_quic_datagram::{allowlist::AllowAll, endpoint::Datagram},
    std::{net::UdpSocket, sync::Arc, time::Duration},
};

fn clone_keypair(k: &Keypair) -> Keypair {
    k.insecure_clone()
}

/// Hold a bound UDP socket that never speaks QUIC. The kernel queues
/// arriving packets in the socket's recv buffer; a peer dialing this
/// address sees no quinn responses and eventually times the handshake out.
fn bind_blackhole() -> UdpSocket {
    bind_to_localhost_unique().expect("bind blackhole socket")
}

#[test]
fn failed_connect_clears_placeholder() {
    // Dial to a blackhole address times out (quinn max_idle = 2s per
    // transport.rs::MAX_IDLE_TIMEOUT). On failure the `Dialing` placeholder is
    // removed so a follow-up egress to the same pubkey at a working addr
    // can spawn a fresh dial and reach the server.
    let rt = make_runtime();
    let server = spawn_node(&rt, Arc::new(AllowAll));
    let s_pubkey = server.pubkey();

    let client = spawn_node_with(&rt, Arc::new(AllowAll), keypair_below(&s_pubkey));

    // Pretend gossip published a bogus addr for `s_pubkey`. The dial will
    // fail; if the placeholder is not cleaned up, the follow-up below
    // would see `Occupied(Dialing)` and drop forever.
    let blackhole = bind_blackhole();
    let bogus_addr = blackhole.local_addr().expect("blackhole addr");

    let p1 = Bytes::from_static(b"p1-blackhole");
    rt.block_on(async {
        client
            .endpoint
            .egress
            .send(Datagram {
                peer_pubkey: s_pubkey,
                peer_address: bogus_addr,
                message: p1.clone(),
            })
            .await
            .unwrap();
    });

    // Give the dial enough time to fail. transport.rs sets
    // max_idle_timeout = 2s, but quinn's handshake PTO + retransmits can
    // run longer than the configured idle before the local watchdog
    // fires; wait a generous margin.
    std::thread::sleep(Duration::from_secs(8));

    let p2 = Bytes::from_static(b"p2-real");
    send_until_received(
        &rt,
        &client.endpoint,
        s_pubkey,
        server.addr,
        p2.clone(),
        &server.ingress_rx,
        Duration::from_secs(5),
        |d| (d.message == p2).then_some(()),
        "server did not receive the retry - placeholder not cleared after dial failure",
    );

    // p1 went to the blackhole; server should never see it. Drain p2
    // retry duplicates, then assert no foreign packet arrives.
    drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
        d.message == p2
    });
    let stray = server.ingress_rx.recv_timeout(Duration::from_millis(500));
    assert!(
        stray.is_err(),
        "server unexpectedly received {stray:?}; p1 should have been lost to the blackhole",
    );
    drop(blackhole);
}

#[test]
fn identity_mismatch_clears_placeholder() {
    // Dial to a server whose attested identity differs from the pubkey
    // the caller targeted: the client closes the conn with INVALID_IDENTITY
    // and removes its `Dialing` placeholder. A follow-up egress with the
    // correct pubkey/addr must succeed.
    let rt = make_runtime();

    // Two servers with distinct keypairs at distinct addrs. We send to
    // (s2_pk, s1.addr) - quinn handshake succeeds, then the attested pk
    // (s1's) does not match the caller-requested s2_pk → INVALID_IDENTITY.
    let s1 = spawn_node(&rt, Arc::new(AllowAll));
    let s2 = spawn_node(&rt, Arc::new(AllowAll));

    // Client must be lex-lower than both servers (lex tiebreak - only the
    // lower side dials).
    let lower_bound = s1.pubkey().min(s2.pubkey());
    let client = spawn_node_with(&rt, Arc::new(AllowAll), keypair_below(&lower_bound));

    let s2_pk = s2.pubkey();
    let s2_addr = s2.addr;
    let bad = Bytes::from_static(b"to-wrong-addr");
    rt.block_on(async {
        client
            .endpoint
            .egress
            .send(Datagram {
                peer_pubkey: s2_pk,
                peer_address: s1.addr,
                message: bad.clone(),
            })
            .await
            .unwrap();
    });

    // Give the bogus dial time to complete its handshake, observe the
    // identity mismatch, and clear the placeholder.
    std::thread::sleep(Duration::from_secs(1));

    // Same pk, now at the real addr. Must succeed.
    let good = Bytes::from_static(b"to-right-addr");
    send_until_received(
        &rt,
        &client.endpoint,
        s2_pk,
        s2_addr,
        good.clone(),
        &s2.ingress_rx,
        Duration::from_secs(5),
        |d| (d.message == good).then_some(()),
        "S2 did not receive retry - placeholder not cleared after INVALID_IDENTITY",
    );

    // S1 must not see either datagram. The bogus one was closed before
    // any data flowed; the good one targeted s2.
    let stray = s1.ingress_rx.recv_timeout(Duration::from_millis(500));
    assert!(stray.is_err(), "S1 unexpectedly received {stray:?}",);
    let _ = clone_keypair(&s1.keypair); // silence unused-fn warning if any
}
