//! A UDP proxy for tests which need to emulate packet losses.
use {
    crate::sockets::bind_to_localhost_unique,
    log::warn,
    std::{
        io::ErrorKind,
        net::SocketAddr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    },
};

/// Socket read timeout, i.e. how fast the relay threads notice `stop`.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Relay buffer size, large enough for any solana packet.
const MAX_DATAGRAM_SIZE: usize = 1500;

/// A UDP relay between a client and a server, either direction of which can be
/// silently blackholed at runtime.
///
/// Point the client at `client_facing_address` instead of the server address.
/// Datagrams are relayed both ways (if server replies).
///
/// The relay owns two sockets: a client-facing one, and one used for all
/// traffic to the server. The client's address is learned from the first datagram
/// it sends, so the client must send first.
pub struct UdpRelay {
    client_facing_address: SocketAddr,
    drop_forward: Arc<AtomicBool>,
    drop_reverse: Arc<AtomicBool>,
    dropped_towards_server: Arc<AtomicU64>,
    dropped_towards_client: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl UdpRelay {
    /// Spawn a relay forwarding to `destination` on localhost.
    pub fn spawn(destination: SocketAddr) -> UdpRelay {
        let to_client = bind_to_localhost_unique().unwrap();
        let to_server = bind_to_localhost_unique().unwrap();
        let client_facing_address = to_client.local_addr().unwrap();
        for socket in [&to_client, &to_server] {
            socket
                .set_read_timeout(Some(POLL_INTERVAL))
                .expect("set read timeout");
        }
        let (to_client, to_server) = (Arc::new(to_client), Arc::new(to_server));
        let drop_forward = Arc::new(AtomicBool::new(false));
        let drop_reverse = Arc::new(AtomicBool::new(false));
        let dropped_towards_server = Arc::new(AtomicU64::new(0));
        let dropped_towards_client = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        // Learned from the first datagram the client sends; the relay has no
        // other way to know where to return the server's traffic.
        let client_addr = Arc::new(Mutex::new(None));

        let forward = {
            let (to_client, to_server) = (Arc::clone(&to_client), Arc::clone(&to_server));
            let drop_forward = Arc::clone(&drop_forward);
            let dropped = Arc::clone(&dropped_towards_server);
            let stop = Arc::clone(&stop);
            let client_addr = Arc::clone(&client_addr);
            thread::Builder::new()
                .name("solRelayFwd".to_string())
                .spawn(move || {
                    let mut buf = [0u8; MAX_DATAGRAM_SIZE];
                    while !stop.load(Ordering::Relaxed) {
                        let (len, src) = match to_client.recv_from(&mut buf) {
                            Ok(received) => received,
                            Err(err) if err.kind() == ErrorKind::WouldBlock => continue,
                            Err(e) => panic!("UDP relay cannot read from the client: {e}"),
                        };
                        assert!(
                            len < MAX_DATAGRAM_SIZE,
                            "datagram from the client filled the whole {MAX_DATAGRAM_SIZE} byte \
                             buffer, so recv_from may have truncated it"
                        );
                        // Track the client even while dropping, so the return
                        // path is ready the moment the direction reopens.
                        *client_addr.lock().expect("client address mutex") = Some(src);
                        if drop_forward.load(Ordering::Relaxed) {
                            dropped.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if let Err(e) = to_server.send_to(&buf[..len], destination) {
                            warn!("UDP relay failed to send to {destination}: {e}");
                        }
                    }
                })
                .unwrap()
        };
        let reverse = {
            let drop_reverse = Arc::clone(&drop_reverse);
            let dropped = Arc::clone(&dropped_towards_client);
            let stop = Arc::clone(&stop);
            let client_addr = Arc::clone(&client_addr);
            thread::Builder::new()
                .name("solRelayRev".to_string())
                .spawn(move || {
                    let mut buf = [0u8; MAX_DATAGRAM_SIZE];
                    while !stop.load(Ordering::Relaxed) {
                        let (len, _src) = match to_server.recv_from(&mut buf) {
                            Ok(received) => received,
                            Err(err) if err.kind() == ErrorKind::WouldBlock => continue,
                            Err(e) => panic!("UDP relay cannot read from the server: {e}"),
                        };
                        assert!(
                            len < MAX_DATAGRAM_SIZE,
                            "datagram from the server filled the whole {MAX_DATAGRAM_SIZE} byte \
                             buffer, so recv_from may have truncated it"
                        );
                        // Nowhere to send until the client has spoken once.
                        let Some(dst) = *client_addr.lock().expect("client address mutex") else {
                            continue;
                        };
                        if drop_reverse.load(Ordering::Relaxed) {
                            dropped.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if let Err(e) = to_client.send_to(&buf[..len], dst) {
                            warn!("UDP relay failed to send to {dst}: {e}");
                        }
                    }
                })
                .unwrap()
        };

        UdpRelay {
            client_facing_address,
            drop_forward,
            drop_reverse,
            dropped_towards_server,
            dropped_towards_client,
            stop,
            threads: vec![forward, reverse],
        }
    }

    pub fn client_facing_address(&self) -> SocketAddr {
        self.client_facing_address
    }

    pub fn set_drop_towards_server(&self, drop: bool) {
        self.drop_forward.store(drop, Ordering::Relaxed);
    }

    pub fn set_drop_towards_client(&self, drop: bool) {
        self.drop_reverse.store(drop, Ordering::Relaxed);
    }

    /// Datagrams from the client discarded since spawn.
    pub fn dropped_towards_server(&self) -> u64 {
        self.dropped_towards_server.load(Ordering::Relaxed)
    }

    /// Datagrams from the server discarded since spawn.
    pub fn dropped_towards_client(&self) -> u64 {
        self.dropped_towards_client.load(Ordering::Relaxed)
    }
}

impl Drop for UdpRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            assert!(
                thread.join().is_ok() || thread::panicking(),
                "UDP relay thread panicked"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_udp_relay() {
        const TIMEOUT: Duration = Duration::from_secs(1);
        let server = bind_to_localhost_unique().expect("bind server socket");
        let client = bind_to_localhost_unique().expect("bind client socket");
        let server_addr = server.local_addr().expect("server local address");
        let client_addr = client.local_addr().expect("client local address");
        for socket in [&server, &client] {
            socket
                .set_read_timeout(Some(TIMEOUT))
                .expect("set read timeout");
        }
        let relay = UdpRelay::spawn(server_addr);

        // One round trip through the relay. The request teaches the relay where
        // the client lives, so a reply can find its way back. Reports which of
        // the two legs made it.
        let mut buf = [0u8; 64];
        let mut round_trip = |request: &[u8], reply: &[u8]| -> (bool, bool) {
            client
                .send_to(request, relay.client_facing_address())
                .expect("client send");
            let Ok((len, src)) = server.recv_from(&mut buf) else {
                return (false, false);
            };
            assert_eq!(&buf[..len], request, "relay must not alter the payload");
            assert_ne!(
                src, client_addr,
                "the server must see the relay as the source, not the client"
            );
            server.send_to(reply, src).expect("server reply");
            let Ok((len, _src)) = client.recv_from(&mut buf) else {
                return (true, false);
            };
            assert_eq!(&buf[..len], reply, "relay must not alter the payload");
            (true, true)
        };

        assert_eq!(
            round_trip(b"ping", b"pong"),
            (true, true),
            "an open relay must pass traffic both ways"
        );
        relay.set_drop_towards_client(true);
        assert_eq!(
            round_trip(b"request", b"dropped-reply"),
            (true, false),
            "only the direction towards the client must be blackholed"
        );
        relay.set_drop_towards_client(false);
        relay.set_drop_towards_server(true);
        assert_eq!(
            round_trip(b"dropped-request", b"never-sent"),
            (false, false),
            "a blackholed direction towards the server must swallow the request"
        );
        relay.set_drop_towards_server(false);
        assert_eq!(
            round_trip(b"ping-again", b"pong-again"),
            (true, true),
            "traffic must resume once both directions are reopened"
        );
        assert_eq!(
            (
                relay.dropped_towards_server(),
                relay.dropped_towards_client()
            ),
            (1, 1),
            "exactly one datagram must have been dropped in each direction"
        );
    }
}
