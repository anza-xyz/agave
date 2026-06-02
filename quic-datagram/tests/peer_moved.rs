//! Caller-driven addr refresh: when a caller (e.g. votor via a gossip
//! refresh) hands the endpoint a new SocketAddr for a pubkey already in
//! the connection table, the lex-lower (dialer) side must evict its
//! stale outbound connection and re-dial the new addr. Lex-higher
//! cached entries are server-accepted (peer's NAT-mapped source addr
//! can legitimately differ from gossip's published one) and are trusted
//! regardless of the caller's claimed addr.

#[path = "common.rs"]
mod common;

use {
    bytes::Bytes,
    common::{
        drain_matching, keypair_below, make_runtime, send_until_received, spawn_node,
        spawn_node_with,
    },
    solana_net_utils::sockets::bind_to_localhost_unique,
    solana_quic_datagram::{admission::AllowAll, endpoint::Datagram},
    std::{sync::Arc, time::Duration},
};

#[test]
fn egress_during_dial_dropped_addr_recovers_after_timeout() {
    // Mid-dial address change: while a dial to addr A1 is still in flight
    // (blackhole'd, will time out), the caller queues an egress with a
    // different addr A2 for the same pubkey. The state-machine drops it
    // (placeholder is `Dialing`, not `Established`, so the addr-mismatch
    // eviction path doesn't apply). Once the first dial times out and
    // clears the placeholder, a fresh egress to A2 must succeed.
    let rt = make_runtime();
    let server = spawn_node(&rt, Arc::new(AllowAll));
    let s_pubkey = server.pubkey();
    let client = spawn_node_with(&rt, Arc::new(AllowAll), keypair_below(&s_pubkey));

    let blackhole = bind_to_localhost_unique().expect("blackhole socket");
    let blackhole_addr = blackhole.local_addr().expect("blackhole addr");

    // 1. start dial to blackhole - server's pubkey, wrong addr.
    let p1 = Bytes::from_static(b"dial-to-blackhole");
    rt.block_on(async {
        client
            .endpoint
            .egress
            .send(Datagram {
                peer_pubkey: s_pubkey,
                peer_address: blackhole_addr,
                message: p1.clone(),
            })
            .await
            .unwrap();
    });
    // Give the dial a beat to install its `Dialing` placeholder.
    std::thread::sleep(Duration::from_millis(200));

    // 2. while `Dialing`, send to the REAL addr. Should be dropped (we do
    // not buffer; one dial task per peer at a time).
    let p2 = Bytes::from_static(b"during-dial-dropped");
    rt.block_on(async {
        client
            .endpoint
            .egress
            .send(Datagram {
                peer_pubkey: s_pubkey,
                peer_address: server.addr,
                message: p2.clone(),
            })
            .await
            .unwrap();
    });
    // Confirm server does NOT see p2 within the dial-in-progress window.
    let stray = server.ingress_rx.recv_timeout(Duration::from_millis(500));
    assert!(
        stray.is_err(),
        "server unexpectedly received {stray:?} while dial-in-flight"
    );

    // 3. wait for the blackhole dial to time out, clearing the placeholder.
    std::thread::sleep(Duration::from_secs(8));

    // 4. retry the real addr - must succeed.
    let p3 = Bytes::from_static(b"post-timeout-retry");
    send_until_received(
        &rt,
        &client.endpoint,
        s_pubkey,
        server.addr,
        p3.clone(),
        &server.ingress_rx,
        Duration::from_secs(5),
        |d| (d.message == p3).then_some(()),
        "server did not receive post-timeout retry",
    );

    // p1 and p2 never reach the server: p1 went to the blackhole, p2 was
    // dropped while `Dialing`. Drain p3 retry duplicates, then assert
    // no foreign packet arrives.
    drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
        d.message == p3
    });
    let stray = server.ingress_rx.recv_timeout(Duration::from_millis(500));
    assert!(
        stray.is_err(),
        "server unexpectedly received {stray:?} after the retry landed",
    );
    drop(blackhole);
}
