//! Per-connection reader task. Shared by server-accepted and client-dialed
//! connections - both push received datagrams into the same ingress channel.

use {
    crate::{
        BAN_DURATION_SHORT, BURST_DATAGRAMS_PER_SECOND_PER_PEER, Banlist,
        MAX_DATAGRAMS_PER_SECOND_PER_PEER, close_codes,
        endpoint::Datagram,
        error::Error,
        stats::{QuicDatagramStats, record_error},
    },
    crossbeam_channel::{Sender, TrySendError},
    log::debug,
    quinn::{Connection, ConnectionError},
    solana_net_utils::token_bucket::TokenBucket,
    solana_pubkey::Pubkey,
    std::{
        net::SocketAddr,
        sync::{Arc, atomic::Ordering},
    },
};

/// Drive the per-connection read loop to completion. Returns when the
/// connection closes (peer-initiated, banlist-trip, ingress disconnect,
/// HANDOVER). Caller is responsible for reaping the connection table entry
/// afterwards.
pub(crate) async fn read_datagram_loop(
    connection: Connection,
    peer: Pubkey,
    remote_addr: SocketAddr,
    ingress: Sender<Datagram>,
    banlist: Arc<Banlist<Pubkey>>,
    stats: Arc<QuicDatagramStats>,
) {
    // Per-connection rate limiter. Any datagram arriving with the bucket
    // empty is dropped. We do NOT close the connection or ban the peer here,
    // honest peers legitimately burst above the refill rate during catch-up.
    let rate_limit = TokenBucket::new(
        BURST_DATAGRAMS_PER_SECOND_PER_PEER,
        BURST_DATAGRAMS_PER_SECOND_PER_PEER,
        MAX_DATAGRAMS_PER_SECOND_PER_PEER,
    );
    loop {
        match connection.read_datagram().await {
            Ok(bytes) => {
                // Banlist check happens AFTER the read so a ban that
                // lands while we're awaiting can't let a follow-up
                // datagram leak through to ingress.
                if banlist.is_banned(&peer) {
                    close_codes::BANNED.close(&connection);
                    break;
                }
                if rate_limit.consume_tokens(1).is_err() {
                    drop(bytes);
                    stats.datagram_rate_limited.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                match ingress.try_send(Datagram {
                    peer_pubkey: peer,
                    peer_address: remote_addr,
                    message: bytes,
                }) {
                    Ok(()) => {
                        stats.datagrams_received.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Full(_)) => {
                        stats
                            .datagram_ingress_dropped_channel_full
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        debug!("ingress disconnected; reader for {peer} exiting");
                        break;
                    }
                }
            }
            Err(e) => {
                if matches!(&e, ConnectionError::ApplicationClosed(c) if c.error_code == close_codes::HANDOVER.code)
                {
                    handle_handover(peer, &banlist, &stats);
                }
                record_error(&Error::from(e), &stats);
                break;
            }
        }
    }
}

/// Soft-ban the peer on `HANDOVER` close so our client side stops dialing
/// it — the peer has accepted a different instance of our identity and
/// re-dialing would risk double-voting.
fn handle_handover(peer: Pubkey, banlist: &Banlist<Pubkey>, stats: &QuicDatagramStats) {
    stats.handover_received.fetch_add(1, Ordering::Relaxed);
    banlist.ban(peer, BAN_DURATION_SHORT);
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            BAN_DURATION_SHORT, BURST_DATAGRAMS_PER_SECOND_PER_PEER,
            allowlist::AllowAll,
            endpoint::Datagram,
            testutils::{
                clone_keypair, drain_matching, keypair_below, make_runtime, recv_until,
                send_until_received, spawn_node, spawn_node_with,
            },
        },
        bytes::Bytes,
        solana_keypair::Signer,
        std::{sync::Arc, time::Duration},
    };

    #[test]
    fn ban_evicts_existing_and_blocks_rehandshake() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll));
        // Lex tiebreaker: dialer's pubkey must be lower than listener's.
        let client = spawn_node_with(&rt, Arc::new(AllowAll), keypair_below(&server.pubkey()));

        // Establish a connection by driving send-until-receive: the trigger
        // packet is dropped; the retry through the now-Established slot lands.
        let probe = Bytes::from_static(b"probe");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            probe.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == probe).then_some(()),
            "first datagram never arrived",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == probe
        });

        // Ban the client at the server side. Eviction task should close the
        // server-side connection; client's read loop sees ConnectionClosed and
        // exits, dropping its cache entry on the way out.
        server.banlist.ban(client.pubkey(), BAN_DURATION_SHORT);

        // Best-effort wait for eviction to flush. The eviction task is async; on
        // a quiet system it observes the channel within a few ms.
        std::thread::sleep(Duration::from_millis(200));

        // Now have the client send again. The send path dials a new connection
        // (server-side cache no longer holds the old one); but the server is
        // banning this pubkey, so handshake closes with BANNED and no datagram
        // ever reaches ingress.
        let again = Bytes::from_static(b"after-ban");
        rt.block_on(async {
            client
                .endpoint
                .egress
                .send(Datagram {
                    peer_pubkey: server.pubkey(),
                    peer_address: server.addr,
                    message: again.clone(),
                })
                .await
                .unwrap();
        });

        let bad = server.ingress_rx.recv_timeout(Duration::from_millis(1500));
        assert!(
            bad.is_err(),
            "banned client must not deliver datagrams to server; got {bad:?}"
        );
    }

    #[test]
    fn second_connection_with_same_keypair_handovers_first() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll));

        // Lex rule: shared client keypair must be lower than server's.
        let shared = keypair_below(&server.pubkey());
        let c_pubkey = shared.pubkey();
        let c1 = spawn_node_with(&rt, Arc::new(AllowAll), clone_keypair(&shared));

        // C1 establishes a connection by driving send-until-receive.
        let p1 = Bytes::from_static(b"p1");
        send_until_received(
            &rt,
            &c1.endpoint,
            server.pubkey(),
            server.addr,
            p1.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == p1).then_some(()),
            "server did not receive c1's probe",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == p1
        });

        // C2 arrives with the same keypair. Its handshake completion at the
        // server triggers HANDOVER on c1's connection.
        let c2 = spawn_node_with(&rt, Arc::new(AllowAll), clone_keypair(&shared));
        let p2 = Bytes::from_static(b"p2");
        send_until_received(
            &rt,
            &c2.endpoint,
            server.pubkey(),
            server.addr,
            p2.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == p2).then_some(()),
            "server did not receive c2's probe",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == p2
        });

        let server_pk = server.pubkey();

        // C1's read loop observes the HANDOVER close and soft-bans the server.
        // Give the async read loop a moment to process the close.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            c1.banlist.is_banned(&server_pk),
            "c1 must soft-ban the server that handovered it"
        );

        // After HANDOVER the server's table holds c2 only. Server's egress to
        // c_pubkey reaches c2; c1 must not see post-handover datagrams.
        let after = Bytes::from_static(b"post-handover");
        rt.block_on(async {
            server
                .endpoint
                .egress
                .send(Datagram {
                    peer_pubkey: c_pubkey,
                    peer_address: c1.addr,
                    message: after.clone(),
                })
                .await
                .unwrap();
        });
        recv_until(
            &c2.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == after).then_some(()),
            "c2 (post-handover) must receive server's datagram",
        );
        let stray = c1.ingress_rx.recv_timeout(Duration::from_millis(800));
        assert!(
            stray.is_err(),
            "c1 was handovered; must not receive post-handover datagrams; got {stray:?}"
        );
    }

    #[test]
    /// Per-connection receive token bucket. A burst beyond
    /// `BURST_DATAGRAMS_PER_SECOND_PER_PEER` is *dropped* silently - the
    /// bucket itself is the throttle. The connection stays alive and the
    /// peer is NOT banned (consensus traffic legitimately bursts above the
    /// refill rate during catch-up).
    fn burst_exceeding_rate_limit_drops_excess_without_banning() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll));
        let client = spawn_node_with(&rt, Arc::new(AllowAll), keypair_below(&server.pubkey()));

        // Establish the connection: retry the probe until one lands (first one
        // triggers the dial, gets dropped; followers ride the Established slot).
        let probe = Bytes::from_static(b"probe");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            probe.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == probe).then_some(()),
            "first datagram never arrived",
        );
        // Drain probe retry duplicates so they don't inflate the post-burst
        // delivery count below.
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == probe
        });

        // Blast a burst far in excess of the bucket capacity. Probe retries
        // have already consumed some tokens; the (capacity)-th additional
        // datagram trips the limiter.
        let burst = (BURST_DATAGRAMS_PER_SECOND_PER_PEER as usize) * 4;
        rt.block_on(async {
            for i in 0..burst {
                let payload = Bytes::from(format!("burst-{i:04}").into_bytes());
                client
                    .endpoint
                    .egress
                    .send(Datagram {
                        peer_pubkey: server.pubkey(),
                        peer_address: server.addr,
                        message: payload,
                    })
                    .await
                    .unwrap();
            }
        });

        // Let the receiver chew through whatever fits in the bucket.
        std::thread::sleep(Duration::from_millis(500));

        // The peer must NOT have been banned - drop-only semantics.
        assert!(
            !server.banlist.is_banned(&client.pubkey()),
            "client pubkey should NOT be banned by RX rate limit (drop-only semantics)"
        );

        // Drain whatever made it through ingress. The bucket caps how many
        // post-probe datagrams can be delivered within the first refill window.
        let mut delivered = 0usize;
        while server
            .ingress_rx
            .recv_timeout(Duration::from_millis(50))
            .is_ok()
        {
            delivered = delivered.saturating_add(1);
        }
        let cap = BURST_DATAGRAMS_PER_SECOND_PER_PEER as usize;
        assert!(
            delivered <= cap + 2,
            "delivered {delivered} datagrams post-probe, exceeds bucket capacity {cap} + slop"
        );

        // After waiting long enough for the bucket to refill, the sender should
        // be able to deliver fresh datagrams on the SAME connection (proves the
        // connection wasn't torn down).
        std::thread::sleep(Duration::from_secs(2));
        let resume = Bytes::from_static(b"after-refill");
        rt.block_on(async {
            client
                .endpoint
                .egress
                .send(Datagram {
                    peer_pubkey: server.pubkey(),
                    peer_address: server.addr,
                    message: resume.clone(),
                })
                .await
                .unwrap();
        });
        recv_until(
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == resume).then_some(()),
            "post-refill datagram never arrived - connection may have been torn down",
        );
    }
}
