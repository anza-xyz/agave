//! End-to-end integration tests for the quic-datagram endpoint.
//!
//! Covers three behaviors that need real quinn endpoints over loopback:
//!   * `Dialing` placeholder cleanup on dial failure / identity mismatch.
//!   * Caller-driven addr refresh while a dial is still in flight.
//!   * Lex-pubkey direction tiebreaker at scale (no duplicate delivery).

use {
    bytes::Bytes,
    solana_keypair::Keypair,
    solana_net_utils::sockets::bind_to_localhost_unique,
    solana_pubkey::Pubkey,
    solana_quic_datagram::{
        allowlist::AllowAll,
        endpoint::Datagram,
        testutils::{
            TestNode, drain_matching, keypair_below, make_runtime, send_until_received, spawn_node,
        },
    },
    std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration},
};

// ---------------------------------------------------------------------------
// `ConnectionTableEntry::Dialing` placeholder semantics.
//
// The lex-lower dialer must clean up its placeholder on dial failure so a
// subsequent egress to the same pubkey can retry (`endpoint.connect` timeout,
// server identity mismatch).
// ---------------------------------------------------------------------------

#[test]
fn failed_connect_clears_placeholder() {
    // Dial to a blackhole address times out (quinn max_idle = 2s per
    // transport.rs::MAX_IDLE_TIMEOUT). On failure the `Dialing` placeholder is
    // removed so a follow-up egress to the same pubkey at a working addr
    // can spawn a fresh dial and reach the server.
    let rt = make_runtime();
    let server = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
    let s_pubkey = server.pubkey();

    let client = spawn_node(&rt, Arc::new(AllowAll), keypair_below(&s_pubkey));

    // Pretend gossip published a bogus addr for `s_pubkey`. The dial will
    // fail; if the placeholder is not cleaned up, the follow-up below
    // would see `Occupied(Dialing)` and drop forever.
    let blackhole = bind_to_localhost_unique().expect("bind blackhole socket");
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

/// Dial to a server whose attested identity differs from the pubkey
/// the caller targeted: the client closes the conn with INVALID_IDENTITY
/// and removes its `Dialing` placeholder. A follow-up egress with the
/// correct pubkey/addr must succeed.
#[test]
fn identity_mismatch_clears_placeholder() {
    let rt = make_runtime();

    // Two servers with distinct keypairs at distinct addrs. We send to
    // (s2_pk, s1.addr) - quinn handshake succeeds, then the attested pk
    // (s1's) does not match the caller-requested s2_pk → INVALID_IDENTITY.
    let s1 = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
    let s2 = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

    // Client must be lex-lower than both servers (lex tiebreak - only the
    // lower side dials).
    let lower_bound = s1.pubkey().min(s2.pubkey());
    let client = spawn_node(&rt, Arc::new(AllowAll), keypair_below(&lower_bound));

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
    let _ = {
        let k: &Keypair = &s1.keypair;
        k.insecure_clone()
    }; // silence unused-fn warning if any
}

// ---------------------------------------------------------------------------
// Caller-driven addr refresh: when a caller (e.g. votor via a gossip refresh)
// hands the endpoint a new SocketAddr for a pubkey already in the connection
// table, the lex-lower (dialer) side must evict its stale outbound connection
// and re-dial the new addr. Lex-higher cached entries are server-accepted
// (peer's NAT-mapped source addr can legitimately differ from gossip's
// published one) and are trusted regardless of the caller's claimed addr.
// ---------------------------------------------------------------------------

#[test]
fn egress_during_dial_dropped_addr_recovers_after_timeout() {
    // Mid-dial address change: while a dial to addr A1 is still in flight
    // (blackhole'd, will time out), the caller queues an egress with a
    // different addr A2 for the same pubkey. The state-machine drops it
    // (placeholder is `Dialing`, not `Established`, so the addr-mismatch
    // eviction path doesn't apply). Once the first dial times out and
    // clears the placeholder, a fresh egress to A2 must succeed.
    let rt = make_runtime();
    let server = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
    let s_pubkey = server.pubkey();
    let client = spawn_node(&rt, Arc::new(AllowAll), keypair_below(&s_pubkey));

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

// ---------------------------------------------------------------------------
// Lex-pubkey direction tiebreaker.
// ---------------------------------------------------------------------------

#[test]
/// Lex-pubkey tiebreaker - verifies the simultaneous-dial race resolves to
/// a single connection per peer in each side's table.
///
/// Setup: N=32 nodes all mutually-admitted, every node sends to every other
/// at the same time.
///
/// Assertion: after warm-up settles, a follow-up "burst" of one unique
/// payload per (sender, receiver) pair must arrive exactly once at every
/// receiver - no duplicates from dual flows.
fn tiebreak_no_duplicates_after_settle() {
    const N: usize = 32;
    let rt = make_runtime();
    let nodes: Vec<TestNode> = (0..N)
        .map(|_| spawn_node(&rt, Arc::new(AllowAll), Keypair::new()))
        .collect();
    let pubkeys: Vec<Pubkey> = nodes.iter().map(|n| n.pubkey()).collect();
    let addrs: Vec<SocketAddr> = nodes.iter().map(|n| n.addr).collect();

    // Warm-up: every node concurrently sends one payload to every other -
    // provokes the simultaneous-dial race for every pair.
    rt.block_on(async {
        let mut tasks = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            let egress = n.endpoint.egress.clone();
            let targets: Vec<Datagram> = (0..N)
                .filter(|&j| j != i)
                .map(|j| Datagram {
                    peer_pubkey: pubkeys[j],
                    peer_address: addrs[j],
                    message: Bytes::from_static(b"warmup"),
                })
                .collect();
            tasks.push(tokio::spawn(async move {
                for t in targets {
                    let _ = egress.send(t).await;
                }
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    });

    // Allow the race to resolve: WRONG_DIRECTION close needs ~1 RTT
    // (localhost: microseconds) plus task scheduling. Generous slack.
    std::thread::sleep(Duration::from_secs(2));

    // Drain warm-up ingress so it can't pollute the burst-phase counts.
    for n in &nodes {
        while n.ingress_rx.try_recv().is_ok() {}
    }

    // Burst: one unique payload per ordered pair. With clean dedup each
    // receiver sees each sender's payload exactly once.
    rt.block_on(async {
        let mut tasks = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            let egress = n.endpoint.egress.clone();
            let targets: Vec<Datagram> = (0..N)
                .filter(|&j| j != i)
                .map(|j| {
                    let payload = Bytes::copy_from_slice(format!("burst-{i}-{j}").as_bytes());
                    Datagram {
                        peer_pubkey: pubkeys[j],
                        peer_address: addrs[j],
                        message: payload,
                    }
                })
                .collect();
            tasks.push(tokio::spawn(async move {
                for t in targets {
                    let _ = egress.send(t).await;
                }
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    });

    std::thread::sleep(Duration::from_secs(1));

    let mut duplicates = Vec::new();
    let mut losses = Vec::new();
    for (j, n) in nodes.iter().enumerate() {
        let mut counts: HashMap<String, usize> = HashMap::new();
        while let Ok(d) = n.ingress_rx.try_recv() {
            let s = String::from_utf8_lossy(&d.message).into_owned();
            *counts.entry(s).or_insert(0) += 1;
        }
        for i in 0..N {
            if i == j {
                continue;
            }
            let key = format!("burst-{i}-{j}");
            let c = counts.get(&key).copied().unwrap_or(0);
            match c {
                1 => {}
                0 => losses.push(format!("node {j}: lost '{key}'")),
                _ => duplicates.push(format!("node {j}: got {c} copies of '{key}'")),
            }
        }
    }

    let total_pairs = N * (N - 1);
    assert!(
        duplicates.is_empty(),
        "{}/{} pairs duplicated (lex tiebreaker failed):\n{}",
        duplicates.len(),
        total_pairs,
        duplicates
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        losses.is_empty(),
        "{}/{} pairs lost their burst datagram (warm-up did not establish a connection):\n{}",
        losses.len(),
        total_pairs,
        losses
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
