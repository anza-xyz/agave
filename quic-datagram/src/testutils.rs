#![cfg(any(test, feature = "dev-context-only-utils"))]
#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        Banlist,
        admission::Admission,
        endpoint::{Datagram, QuicDatagramEndpoint},
    },
    bytes::Bytes,
    crossbeam_channel::Receiver,
    solana_keypair::{Keypair, Signer},
    solana_net_utils::sockets::bind_to_localhost_unique,
    solana_pubkey::Pubkey,
    std::{net::SocketAddr, sync::Arc, time::Duration},
    tokio::runtime::Runtime,
};

pub const TEST_ALPN: &[u8] = b"votor-test";

pub struct TestNode {
    pub keypair: Keypair,
    pub addr: SocketAddr,
    pub endpoint: QuicDatagramEndpoint,
    pub ingress_rx: Receiver<Datagram>,
    pub banlist: Arc<Banlist<Pubkey>>,
}

impl TestNode {
    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }
}

pub fn make_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio multi-thread runtime")
}

/// Spawn an endpoint with `admission` policy and a fresh keypair.
pub fn spawn_node<A: Admission>(rt: &Runtime, admission: Arc<A>) -> TestNode {
    spawn_node_with(rt, admission, Keypair::new())
}

/// Spawn with caller-supplied keypair (lets the test write a server admit-set
/// keyed on the same pubkey).
pub fn spawn_node_with<A: Admission>(
    rt: &Runtime,
    admission: Arc<A>,
    keypair: Keypair,
) -> TestNode {
    let socket = bind_to_localhost_unique().expect("bind UDP socket");
    let addr = socket.local_addr().expect("local addr");
    let (ingress_tx, ingress_rx) = crossbeam_channel::bounded(4096);
    let banlist = Arc::new(Banlist::<Pubkey>::default());
    let endpoint = QuicDatagramEndpoint::new(
        rt.handle(),
        &keypair,
        socket,
        TEST_ALPN,
        ingress_tx,
        admission,
        banlist.clone(),
    )
    .expect("QuicDatagramEndpoint::new");
    TestNode {
        keypair,
        addr,
        endpoint,
        ingress_rx,
        banlist,
    }
}

/// Pull from `rx` until `cond` returns `Some(T)` or `timeout` elapses. Panics
/// with `msg` on timeout.
pub fn recv_until<T>(
    rx: &Receiver<Datagram>,
    timeout: Duration,
    mut cond: impl FnMut(&Datagram) -> Option<T>,
    msg: &str,
) -> T {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(item) => {
                if let Some(t) = cond(&item) {
                    return t;
                }
            }
            Err(_) => break,
        }
    }
    panic!("{msg}");
}

/// Drive a send + receive round-trip, re-sending the payload every
/// `POLL_INTERVAL` until `cond` returns `Some(T)` or `timeout` elapses.
///
/// The first egress of a fresh peer triggers a dial; the trigger
/// packet and any followers that arrive while `Dialing` is in flight
/// are dropped on the floor. So single-shot tests need to keep
/// re-sending until the dial completes and the cached `Established`
/// can carry the payload. Use this in place of
/// `rt.block_on(egress.send(...)) + recv_until(...)` whenever the
/// payload must actually reach the receiver.
///
/// Once `cond` fires, the matching item is returned. Stray duplicates
/// of `payload` may sit in `rx` afterwards (one per retry that landed
/// after the dial completed); drain them if the test cares.
pub fn send_until_received<T>(
    rt: &Runtime,
    endpoint: &QuicDatagramEndpoint,
    target_pk: Pubkey,
    target_addr: SocketAddr,
    payload: Bytes,
    rx: &Receiver<Datagram>,
    timeout: Duration,
    mut cond: impl FnMut(&Datagram) -> Option<T>,
    msg: &str,
) -> T {
    const POLL_INTERVAL: Duration = Duration::from_millis(100);
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        rt.block_on(async {
            let _ = endpoint.egress.try_send(Datagram {
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

/// Drain any backlog from `rx` that matches `pred` within `timeout`.
/// Used after [`send_until_received`] when the test needs to assert
/// "no further packets" — by then `rx` may hold retry duplicates that
/// landed after the dial completed.
pub fn drain_matching(
    rx: &Receiver<Datagram>,
    timeout: Duration,
    mut pred: impl FnMut(&Datagram) -> bool,
) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(item) if pred(&item) => continue,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

/// Generate a fresh keypair whose pubkey is strictly less than `upper`.
///
/// Required by client→server tests under the lex-pubkey tiebreaker: the
/// dialer's pubkey must be lower than the listener's, otherwise the server
/// rejects the inbound with `WRONG_DIRECTION` and the test would observe a
/// silent connection failure.
pub fn keypair_below(upper: &Pubkey) -> Keypair {
    loop {
        let k = Keypair::new();
        if &k.pubkey() < upper {
            return k;
        }
    }
}

pub fn clone_keypair(k: &Keypair) -> Keypair {
    k.insecure_clone()
}
