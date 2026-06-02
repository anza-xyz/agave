#![allow(clippy::arithmetic_side_effects)]
//! Identity rotation exercised through the votor wrapper.
//!
//! The underlying `KeyUpdater` is tested at the quic-datagram crate
//! level (`solana_quic_datagram::tests::identity_rotation`). This test
//! confirms the wrapper at [`agave_votor::datagram_endpoint::spawn`]
//! surfaces the same handle correctly: rotating identity through
//! `key_updater.update_key(&new_kp)` should evict cached connections
//! with `IDENTITY_ROTATED`, swap the local pubkey for the lex rule,
//! and accept subsequent traffic attributed to the new identity.
//!
//! **Production use case context** — this isn't a "rotate to a freshly
//! generated pubkey" scenario. It's a hot-spare failover: a non-voting
//! backup running with a throwaway keypair gets promoted by being
//! handed the staked keypair already in use by the primary. After
//! rotation:
//!   - The new pubkey is already in every peer's admission set (it
//!     was staked the whole time — it's the primary's identity).
//!   - Peers observe a second connection from the same staked pubkey
//!     and replace the primary's connection via HANDOVER.
//!   - The primary receives HANDOVER closes and soft-bans the evicting
//!     peers so it doesn't redial and disrupt consensus.
//!
//! This test uses two arbitrary keypairs (K1 → K2) with `AllowAll`
//! admission on the server side; that exercises the rotation
//! mechanics in isolation without modeling the staked-set realities
//! of a real cluster.

use {
    agave_votor::datagram_endpoint,
    bytes::Bytes,
    crossbeam_channel::Receiver,
    solana_keypair::{Keypair, Signer},
    solana_net_utils::sockets::bind_to_localhost_unique,
    solana_pubkey::Pubkey,
    solana_quic_datagram::{
        Banlist, StakedNodesAdmission, admission::AllowAll, endpoint::Datagram,
    },
    solana_tls_utils::NotifyKeyUpdate,
    std::{
        collections::HashMap,
        sync::Arc,
        time::{Duration, Instant},
    },
    tokio::runtime::Runtime,
};

/// Pick a keypair whose pubkey is strictly less than `upper`. Used so
/// the client always plays the lex-correct dialer role.
fn keypair_below(upper: &Pubkey) -> Keypair {
    loop {
        let k = Keypair::new();
        if &k.pubkey() < upper {
            return k;
        }
    }
}

/// Re-send `payload` on `egress` every 50ms until `rx` observes an
/// item satisfying `cond`, or `timeout` elapses. Mirrors the helper in
/// `quic-datagram/tests/common.rs`: the first egress to a fresh peer
/// is consumed as the dial trigger and dropped; subsequent retries
/// ride the resulting `Established`. Identity rotation evicts the
/// table, so the post-rotation send also needs the same retry loop.
fn send_until_received<T>(
    rt: &Runtime,
    egress: &tokio::sync::mpsc::Sender<Datagram>,
    target_pk: Pubkey,
    target_addr: std::net::SocketAddr,
    payload: Bytes,
    rx: &Receiver<Datagram>,
    timeout: Duration,
    mut cond: impl FnMut(&Datagram) -> Option<T>,
    msg: &str,
) -> T {
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        rt.block_on(async {
            let _ = egress.try_send(Datagram {
                peer_pubkey: target_pk,
                peer_address: target_addr,
                message: payload.clone(),
            });
        });
        if let Ok(item) = rx.recv_timeout(POLL_INTERVAL)
            && let Some(t) = cond(&item)
        {
            return t;
        }
    }
    panic!("{msg}");
}

#[test]
fn identity_rotation_via_votor_wrapper() {
    let rt = Runtime::new().expect("tokio runtime");

    // Server endpoint with AllowAll admission so any client pubkey is
    // accepted — we're testing the rotation path, not admission.
    let server_kp = Keypair::new();
    let server_pubkey = server_kp.pubkey();
    let server_socket = bind_to_localhost_unique().expect("server bind");
    let server_addr = server_socket.local_addr().expect("server addr");
    let (server_ingress_tx, server_ingress_rx) = crossbeam_channel::bounded(4096);
    let server_banlist = Arc::new(Banlist::<Pubkey>::default());
    let server = datagram_endpoint::spawn(
        rt.handle(),
        &server_kp,
        server_socket,
        server_ingress_tx,
        Arc::new(AllowAll),
        server_banlist,
    )
    .expect("server endpoint");

    // Client endpoint with K1 — strictly below the server so the
    // lex-correct dial goes client → server.
    let k1 = keypair_below(&server_pubkey);
    let k1_pubkey = k1.pubkey();
    let client_socket = bind_to_localhost_unique().expect("client bind");
    let (client_ingress_tx, _client_ingress_rx) = crossbeam_channel::bounded(4096);
    let client_banlist = Arc::new(Banlist::<Pubkey>::default());
    // Client admission must include server_pubkey so its dial-side
    // admission check passes. Use a StakedNodesAdmission populated
    // with the server's pubkey (mirrors how the cache would seed it
    // from BankForks's staked_nodes).
    let admit: HashMap<_, _> = std::iter::once((server_pubkey, 100u64)).collect();
    let client = datagram_endpoint::spawn(
        rt.handle(),
        &k1,
        client_socket,
        client_ingress_tx,
        Arc::new(StakedNodesAdmission::new(admit)),
        client_banlist,
    )
    .expect("client endpoint");

    // Send one message under K1. Server should observe it attributed
    // to K1's pubkey. Retry until landed: the first send to a fresh
    // peer is the dial trigger and is dropped.
    let p1 = Bytes::from_static(b"under-K1");
    send_until_received(
        &rt,
        &client.egress,
        server_pubkey,
        server_addr,
        p1.clone(),
        &server_ingress_rx,
        Duration::from_secs(5),
        |d| (d.peer_pubkey == k1_pubkey && d.message == p1).then_some(()),
        "server never received message attributed to K1",
    );

    // Rotate to K2 via the wrapper's KeyUpdater handle. K2 must also
    // be lex-below the server so the dial after rotation still goes
    // the right direction.
    let k2 = keypair_below(&server_pubkey);
    let k2_pubkey = k2.pubkey();
    assert_ne!(k1_pubkey, k2_pubkey, "K1 and K2 must differ");
    NotifyKeyUpdate::update_key(client.key_updater.as_ref(), &k2)
        .expect("key updater accepts rotation");

    // Give the control loop a beat to apply the rotation (rebuild TLS
    // configs, swap, evict the K1 connection).
    std::thread::sleep(Duration::from_millis(500));

    // Send under K2. Server should now observe K2's pubkey. Same retry
    // pattern: rotation wiped the table, so the post-rotation egress is
    // also a dial trigger and is dropped on the first try.
    let p2 = Bytes::from_static(b"under-K2");
    send_until_received(
        &rt,
        &client.egress,
        server_pubkey,
        server_addr,
        p2.clone(),
        &server_ingress_rx,
        Duration::from_secs(5),
        |d| (d.peer_pubkey == k2_pubkey && d.message == p2).then_some(()),
        "server never received message attributed to K2 after rotation",
    );

    // Identity rotation is the local endpoint closing its own
    // connections with IDENTITY_ROTATED — not a HANDOVER event for
    // the peer. The peer's read loop reaps the entry without soft-ban
    // and accepts the fresh K2 handshake on the next send.
    drop(client);
    drop(server);
}
