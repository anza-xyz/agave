use {
    crate::cluster_info_metrics::should_report_message_signature,
    lru::LruCache,
    rand::{CryptoRng, Rng},
    serde::{Deserialize, Serialize},
    serde_big_array::BigArray,
    siphasher::sip::SipHasher24,
    solana_hash::Hash,
    solana_keypair::{signable::Signable, Keypair},
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    solana_signature::Signature,
    solana_signer::Signer,
    std::{
        borrow::Cow,
        hash::{Hash as _, Hasher},
        net::{IpAddr, SocketAddr},
        time::{Duration, Instant},
    },
};

const KEY_REFRESH_CADENCE: Duration = Duration::from_secs(60);
const PING_PONG_HASH_PREFIX: &[u8] = "SOLANA_PING_PONG".as_bytes();
const PONG_SIGNATURE_SAMPLE_LEADING_ZEROS: u32 = 5;

// For backward compatibility we are using a const generic parameter here.
// N should always be >= 8 and only the first 8 bytes are used. So the new code
// should only use N == 8.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Deserialize, Serialize)]
pub struct Ping<const N: usize> {
    from: Pubkey,
    #[serde(with = "BigArray")]
    token: [u8; N],
    signature: Signature,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Deserialize, Serialize)]
pub struct Pong {
    from: Pubkey,
    hash: Hash, // Hash of received ping token.
    signature: Signature,
}

/// Maintains records of remote nodes which have returned a valid response to a
/// ping message, and on-the-fly ping messages pending a pong response from the
/// remote node.
/// Const generic parameter N corresponds to token size in Ping<N> type.
pub struct PingCache<const N: usize> {
    // Time-to-live of received pong messages.
    ttl: Duration,
    // Rate limit delay to generate pings for a given address
    rate_limit_delay: Duration,
    // Hashers initialized with random keys, rotated at KEY_REFRESH_CADENCE.
    // Because at the moment that the keys are rotated some pings might already
    // be in the flight, we need to keep the two most recent hashers.
    hashers: [SipHasher24; 2],
    // When hashers were last refreshed.
    key_refresh: Instant,
    // Timestamp of last ping message sent to a remote socket address.
    pings: LruCache<SocketAddr, Instant>,
    // Verified pong responses from remote socket addresses.
    // Keyed by SocketAddr to detect port changes (if port changes, no pong found, so we reping).
    pongs: LruCache<SocketAddr, Instant>,
    ping_times: LruCache<IpAddr, Instant>,
}

impl<const N: usize> Ping<N> {
    pub fn new(token: [u8; N], keypair: &Keypair) -> Self {
        let signature = keypair.sign_message(&token);
        Ping {
            from: keypair.pubkey(),
            token,
            signature,
        }
    }
}

impl<const N: usize> Sanitize for Ping<N> {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        self.from.sanitize()?;
        // TODO Add self.token.sanitize()?; when rust's
        // specialization feature becomes stable.
        self.signature.sanitize()
    }
}

impl<const N: usize> Signable for Ping<N> {
    #[inline]
    fn pubkey(&self) -> Pubkey {
        self.from
    }

    #[inline]
    fn signable_data(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.token)
    }

    #[inline]
    fn get_signature(&self) -> Signature {
        self.signature
    }

    fn set_signature(&mut self, signature: Signature) {
        self.signature = signature;
    }
}

impl Pong {
    pub fn new<const N: usize>(ping: &Ping<N>, keypair: &Keypair) -> Self {
        let hash = hash_ping_token(&ping.token);
        Pong {
            from: keypair.pubkey(),
            hash,
            signature: keypair.sign_message(hash.as_ref()),
        }
    }

    pub fn from(&self) -> &Pubkey {
        &self.from
    }

    pub(crate) fn signature(&self) -> &Signature {
        &self.signature
    }
}

impl Sanitize for Pong {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        self.from.sanitize()?;
        self.hash.sanitize()?;
        self.signature.sanitize()
    }
}

impl Signable for Pong {
    fn pubkey(&self) -> Pubkey {
        self.from
    }

    fn signable_data(&self) -> Cow<'static, [u8]> {
        Cow::Owned(self.hash.as_ref().into())
    }

    fn get_signature(&self) -> Signature {
        self.signature
    }

    fn set_signature(&mut self, signature: Signature) {
        self.signature = signature;
    }
}

impl<const N: usize> PingCache<N> {
    pub fn new<R: Rng + CryptoRng>(
        rng: &mut R,
        now: Instant,
        ttl: Duration,
        rate_limit_delay: Duration,
        cap: usize,
    ) -> Self {
        // Sanity check ttl/rate_limit_delay
        assert!(rate_limit_delay <= ttl / 2);
        Self {
            ttl,
            rate_limit_delay,
            hashers: std::array::from_fn(|_| SipHasher24::new_with_key(&rng.random())),
            key_refresh: now,
            pings: LruCache::new(cap),
            pongs: LruCache::new(cap),
            ping_times: LruCache::new(cap),
        }
    }

    /// Checks if the pong hash and socket match a ping message sent
    /// out previously. If so records current timestamp for the remote node and
    /// returns true.
    /// Note: Does not verify the signature.
    pub fn add(&mut self, pong: &Pong, socket: SocketAddr, now: Instant) -> bool {
        let remote_node = (pong.pubkey(), socket);
        if !self.hashers.iter().copied().any(|hasher| {
            let token = make_ping_token::<N>(hasher, &remote_node);
            hash_ping_token(&token) == pong.hash
        }) {
            return false;
        };
        self.pongs.put(socket, now);
        if let Some(sent_time) = self.ping_times.pop(&socket.ip()) {
            if should_report_message_signature(
                pong.signature(),
                PONG_SIGNATURE_SAMPLE_LEADING_ZEROS,
            ) {
                let rtt = now.saturating_duration_since(sent_time);
                datapoint_info!(
                    "ping_rtt",
                    ("peer_ip", socket.ip().to_string(), String),
                    ("rtt_us", rtt.as_micros() as i64, i64),
                );
            }
        }
        true
    }

    /// Checks if the remote socket has been pinged recently. If not, calls the
    /// given function to generates a new ping message, records current
    /// timestamp and hash of ping token, and returns the ping message.
    fn maybe_ping<R: Rng + CryptoRng>(
        &mut self,
        rng: &mut R,
        keypair: &Keypair,
        now: Instant,
        remote_node: (Pubkey, SocketAddr),
    ) -> Option<Ping<N>> {
        let socket = remote_node.1;

        // Always rate limit by socket to prevent spamming the same node when we
        // receive their contact info multiple times (even if we have a valid pong).
        if matches!(self.pings.peek(&socket),
            Some(&t) if now.saturating_duration_since(t) < self.rate_limit_delay)
        {
            return None;
        }

        // Check if we have a valid pong for this socket. If we do, we trust it
        // and skip IP-based rate limiting to allow legitimate nodes on the same IP.
        let has_valid_pong = self
            .pongs
            .peek(&socket)
            .map(|&t| now.saturating_duration_since(t) <= self.ttl)
            .unwrap_or(false);

        // Only rate limit by IP if we don't have a valid pong.
        // This prevents DoS attacks: an attacker sends contact info with victim IP,
        // we ping, victim responds with pong (which fails validation), so we don't
        // have a valid pong and are rate limited from pinging that IP again.
        if !has_valid_pong {
            let ip = socket.ip();
            if matches!(self.ping_times.peek(&ip),
                Some(&t) if now.saturating_duration_since(t) < self.rate_limit_delay)
            {
                return None;
            }
        }

        self.pings.put(socket, now);
        self.maybe_refresh_key(rng, now);
        let token = make_ping_token::<N>(self.hashers[0], &remote_node);
        self.ping_times.put(socket.ip(), now);
        Some(Ping::new(token, keypair))
    }

    /// Returns true if the remote socket has responded to a ping message.
    /// Removes expired pong messages. In order to extend verification before
    /// expiration, if the pong message is not too recent, and the node has not
    /// been pinged recently, calls the given function to generates a new ping
    /// message, records current timestamp and hash of ping token, and returns
    /// the ping message.
    /// Caller should verify if the socket address is valid. (e.g. by using
    /// ContactInfo::is_valid_address).
    pub fn check<R: Rng + CryptoRng>(
        &mut self,
        rng: &mut R,
        keypair: &Keypair,
        now: Instant,
        remote_node: (Pubkey, SocketAddr),
    ) -> (bool, Option<Ping<N>>) {
        let socket = remote_node.1;
        let (check, should_ping) = match self.pongs.get(&socket) {
            None => (false, true),
            Some(t) => {
                let age = now.saturating_duration_since(*t);
                // Pop if the pong message has expired.
                if age > self.ttl {
                    self.pongs.pop(&remote_node);
                    (false, true)
                } else {
                    // If the pong message is not too recent, generate a new ping
                    // message to extend remote node verification. If the pong message is too recent,
                    (true, age > self.ttl / 8)
                }
            }
        };
        let ping = should_ping
            .then(|| self.maybe_ping(rng, keypair, now, remote_node))
            .flatten();
        (check, ping)
    }

    fn maybe_refresh_key<R: Rng + CryptoRng>(&mut self, rng: &mut R, now: Instant) {
        if now.checked_duration_since(self.key_refresh) > Some(KEY_REFRESH_CADENCE) {
            let hasher = SipHasher24::new_with_key(&rng.random());
            self.hashers[1] = std::mem::replace(&mut self.hashers[0], hasher);
            self.key_refresh = now;
        }
    }

    /// Only for tests and simulations.
    pub fn mock_pong(&mut self, _node: Pubkey, socket: SocketAddr, now: Instant) {
        self.pongs.put(socket, now);
    }
}

fn make_ping_token<const N: usize>(
    mut hasher: SipHasher24,
    remote_node: &(Pubkey, SocketAddr),
) -> [u8; N] {
    // TODO: Consider including local node's (pubkey, socket-addr).
    remote_node.hash(&mut hasher);
    let hash = hasher.finish().to_le_bytes();
    debug_assert!(N >= std::mem::size_of::<u64>());
    let mut token = [0u8; N];
    token[..std::mem::size_of::<u64>()].copy_from_slice(&hash);
    token
}

fn hash_ping_token<const N: usize>(token: &[u8; N]) -> Hash {
    solana_sha256_hasher::hashv(&[PING_PONG_HASH_PREFIX, token])
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{
            collections::HashSet,
            iter::repeat_with,
            net::{Ipv4Addr, SocketAddrV4},
        },
    };

    #[test]
    fn test_ping_pong() {
        let mut rng = rand::rng();
        let keypair = Keypair::new();
        let ping = Ping::<32>::new(rng.random(), &keypair);
        assert!(ping.verify());
        assert!(ping.sanitize().is_ok());

        let pong = Pong::new(&ping, &keypair);
        assert!(pong.verify());
        assert!(pong.sanitize().is_ok());
        assert_eq!(
            solana_sha256_hasher::hashv(&[PING_PONG_HASH_PREFIX, &ping.token]),
            pong.hash
        );
    }

    #[test]
    fn test_ping_cache() {
        let now = Instant::now();
        let mut rng = rand::rng();
        let ttl = Duration::from_millis(256);
        let delay = ttl / 64;
        let mut cache = PingCache::new(&mut rng, Instant::now(), ttl, delay, /*cap=*/ 1000);
        let this_node = Keypair::new();
        let keypairs: Vec<_> = repeat_with(Keypair::new).take(8).collect();
        let sockets: Vec<_> = repeat_with(|| {
            SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(rng.random(), rng.random(), rng.random(), rng.random()),
                rng.random(),
            ))
        })
        .take(8)
        .collect();
        let remote_nodes: Vec<(&Keypair, SocketAddr)> = repeat_with(|| {
            let keypair = &keypairs[rng.random_range(0..keypairs.len())];
            let socket = sockets[rng.random_range(0..sockets.len())];
            (keypair, socket)
        })
        .take(128)
        .collect();

        // Initially all checks should fail. The first observation of each socket
        // should create a ping packet.
        let mut seen_sockets = HashSet::<SocketAddr>::new();
        let pings: Vec<Option<Ping<32>>> = remote_nodes
            .iter()
            .map(|(keypair, socket)| {
                let node = (keypair.pubkey(), *socket);
                let (check, ping) = cache.check(&mut rng, &this_node, now, node);
                assert!(!check);
                assert_eq!(seen_sockets.insert(*socket), ping.is_some());
                ping
            })
            .collect();

        let now = now + Duration::from_millis(1);
        for ((keypair, socket), ping) in remote_nodes.iter().zip(&pings) {
            match ping {
                None => {
                    // Already have a recent ping packets for sockets, so no new
                    // ping packet will be generated.
                    let node = (keypair.pubkey(), *socket);
                    let (check, ping) = cache.check(&mut rng, &this_node, now, node);
                    assert!(check);
                    assert!(ping.is_none());
                }
                Some(ping) => {
                    let pong = Pong::new(ping, keypair);
                    assert!(cache.add(&pong, *socket, now));
                }
            }
        }

        let now = now + Duration::from_millis(1);
        // All sockets now have a recent pong packet.
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(check);
            assert!(ping.is_none());
        }

        let now = now + ttl / 8;
        // All sockets still have a valid pong packet, but the cache will create
        // a new ping packet to extend verification.
        seen_sockets.clear();
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(check);
            assert_eq!(seen_sockets.insert(*socket), ping.is_some());
        }

        let now = now + Duration::from_millis(1);
        // All sockets still have a valid pong packet, and a very recent ping
        // packet pending response. So no new ping packet will be created.
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(check);
            assert!(ping.is_none());
        }

        let now = now + ttl;
        // Pong packets have expired. The first observation of each node will
        // remove the expired pong packet from cache and create a new ping packet.
        // check should be false because the pong is expired
        seen_nodes.clear();
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            if seen_nodes.insert(node) {
                assert!(!check, "Expired pong should return check=false");
                assert!(
                    ping.is_some(),
                    "Should generate ping to re-verify expired node"
                );
            } else {
                assert!(!check);
                assert!(ping.is_none());
            }
        }

        let now = now + Duration::from_millis(1);
        // No valid pong packet in the cache. A recent ping packet already
        // created, so no new one will be created.
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(!check);
            assert!(ping.is_none());
        }

        let now = now + ttl / 64;
        // No valid pong packet in the cache. Another ping packet will be
        // created for the first observation of each socket.
        seen_sockets.clear();
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(!check);
            assert_eq!(seen_sockets.insert(*socket), ping.is_some());
        }
    }

    #[test]
    fn test_ip_rate_limiting_no_valid_pong() {
        // Test that IP-based rate limiting prevents rapid pings to the same IP
        // when there's no valid pong.
        let now = Instant::now();
        let mut rng = rand::rng();
        let ttl = Duration::from_millis(256);
        let delay = Duration::from_millis(10); // Short delay for testing
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 1000);
        let this_node = Keypair::new();
        let remote_keypair = Keypair::new();
        let ip = Ipv4Addr::new(192, 168, 1, 1);
        
        // Create two sockets on the same IP but different ports
        let socket1 = SocketAddr::V4(SocketAddrV4::new(ip, 8000));
        let socket2 = SocketAddr::V4(SocketAddrV4::new(ip, 8001));
        
        let node1 = (remote_keypair.pubkey(), socket1);
        let node2 = (remote_keypair.pubkey(), socket2);
        
        // First ping to socket1 should succeed
        let (check, ping1) = cache.check(&mut rng, &this_node, now, node1);
        assert!(!check);
        assert!(ping1.is_some());
        
        // Immediately try to ping socket2 (same IP, different port)
        // Should be rate limited by IP since we don't have a valid pong
        let (check, ping2) = cache.check(&mut rng, &this_node, now, node2);
        assert!(!check);
        assert!(ping2.is_none(), "Should be rate limited by IP");
        
        // After delay passes, should be able to ping socket2
        let now = now + delay + Duration::from_millis(1);
        let (check, ping2) = cache.check(&mut rng, &this_node, now, node2);
        assert!(!check);
        assert!(ping2.is_some(), "Should be able to ping after delay");
    }

    #[test]
    fn test_ip_rate_limiting_with_valid_pong() {
        // Test that when we have a valid pong from any socket on an IP,
        // we can ping other ports on that IP without IP-level rate limiting.
        let now = Instant::now();
        let mut rng = rand::rng();
        let ttl = Duration::from_millis(256);
        let delay = Duration::from_millis(10);
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 1000);
        let this_node = Keypair::new();
        let remote_keypair = Keypair::new();
        let ip = Ipv4Addr::new(192, 168, 1, 1);
        
        let socket1 = SocketAddr::V4(SocketAddrV4::new(ip, 8000));
        let socket2 = SocketAddr::V4(SocketAddrV4::new(ip, 8001));
        
        let node1 = (remote_keypair.pubkey(), socket1);
        let node2 = (remote_keypair.pubkey(), socket2);
        
        // Ping socket1 and get a pong back
        let (check, ping1) = cache.check(&mut rng, &this_node, now, node1);
        assert!(!check);
        assert!(ping1.is_some());
        
        let pong1 = Pong::new(ping1.as_ref().unwrap(), &remote_keypair);
        let now = now + Duration::from_millis(1);
        assert!(cache.add(&pong1, socket1, now));
        
        // Now we have a valid pong from socket1. We should be able to ping socket2
        // immediately without IP-level rate limiting (but socket-level rate limiting
        // still applies, so we need to wait for socket1's rate limit to pass)
        let now = now + delay + Duration::from_millis(1);
        let (check, ping2) = cache.check(&mut rng, &this_node, now, node2);
        assert!(!check);
        assert!(ping2.is_some(), "Should be able to ping different port on same IP when valid pong exists");
    }

    #[test]
    fn test_socket_rate_limiting_always_applies() {
        // Test that socket-level rate limiting always applies, even when we have
        // a valid pong from another socket on the same IP.
        let now = Instant::now();
        let mut rng = rand::rng();
        let ttl = Duration::from_millis(256);
        let delay = Duration::from_millis(10);
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 1000);
        let this_node = Keypair::new();
        let remote_keypair = Keypair::new();
        let ip = Ipv4Addr::new(192, 168, 1, 1);
        
        let socket1 = SocketAddr::V4(SocketAddrV4::new(ip, 8000));
        let socket2 = SocketAddr::V4(SocketAddrV4::new(ip, 8001));
        
        let node1 = (remote_keypair.pubkey(), socket1);
        let node2 = (remote_keypair.pubkey(), socket2);
        
        // Ping socket1 and get a pong back
        let (_check, ping1) = cache.check(&mut rng, &this_node, now, node1);
        assert!(ping1.is_some());
        let pong1 = Pong::new(ping1.as_ref().unwrap(), &remote_keypair);
        let now = now + Duration::from_millis(1);
        assert!(cache.add(&pong1, socket1, now));
        
        // Ping socket2 (should succeed since we have valid pong from socket1)
        let now = now + delay + Duration::from_millis(1);
        let (_check, ping2) = cache.check(&mut rng, &this_node, now, node2);
        assert!(ping2.is_some());
        
        // Immediately try to ping socket2 again - should be rate limited by socket
        let (_check, ping2_again) = cache.check(&mut rng, &this_node, now, node2);
        assert!(ping2_again.is_none(), "Should be rate limited by socket");
        
        // But we can still ping socket1 (different socket, so socket-level rate limit doesn't apply)
        // However, socket1 was pinged at the start, so we need to wait for its rate limit.
        // Also need to wait long enough that the pong age triggers a reping (ttl/8).
        let now = now + delay + Duration::from_millis(1);
        // Wait until pong age > ttl/8 to trigger reping
        let now = now + ttl / 8 + Duration::from_millis(1);
        let (_check, ping1_again) = cache.check(&mut rng, &this_node, now, node1);
        assert!(ping1_again.is_some(), "Should be able to ping socket1 after delay and pong age");
    }

    #[test]
    fn test_ip_ping_info_tracking() {
        // Test that IP ping info is correctly tracked and updated.
        let now = Instant::now();
        let mut rng = rand::rng();
        let ttl = Duration::from_millis(256);
        let delay = Duration::from_millis(10);
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 1000);
        let this_node = Keypair::new();
        let remote_keypair = Keypair::new();
        let ip = Ipv4Addr::new(192, 168, 1, 1);
        let ip_addr = IpAddr::V4(ip);
        let socket = SocketAddr::V4(SocketAddrV4::new(ip, 8000));
        let node = (remote_keypair.pubkey(), socket);
        
        // Initially, no IP ping info should exist
        assert!(cache.ip_ping_info.peek(&ip_addr).is_none());
        
        // Ping the socket
        let (_check, ping) = cache.check(&mut rng, &this_node, now, node);
        assert!(ping.is_some());
        
        // After ping, IP ping info should exist with last_ping_sent set
        let info = cache.ip_ping_info.peek(&ip_addr);
        assert!(info.is_some());
        assert!(info.unwrap().last_ping_sent.is_some());
        assert!(info.unwrap().last_valid_pong.is_none());
        
        // Receive pong
        let pong = Pong::new(ping.as_ref().unwrap(), &remote_keypair);
        let now = now + Duration::from_millis(1);
        assert!(cache.add(&pong, socket, now));
        
        // After pong, IP ping info should have both last_ping_sent and last_valid_pong
        let info = cache.ip_ping_info.peek(&ip_addr);
        assert!(info.is_some());
        assert!(info.unwrap().last_ping_sent.is_some());
        assert!(info.unwrap().last_valid_pong.is_some());
    }

    #[test]
    fn test_ip_ping_info_eviction_handling() {
        // Test that when ip_ping_info entry is evicted and we receive a pong,
        // we handle it correctly with last_ping_sent as None.
        let now = Instant::now();
        let mut rng = rand::rng();
        let ttl = Duration::from_millis(256);
        let delay = Duration::from_millis(10);
        // Use small capacity to force evictions
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 2);
        let this_node = Keypair::new();
        let remote_keypair = Keypair::new();
        let ip = Ipv4Addr::new(192, 168, 1, 1);
        let ip_addr = IpAddr::V4(ip);
        let socket = SocketAddr::V4(SocketAddrV4::new(ip, 8000));
        let node = (remote_keypair.pubkey(), socket);
        
        // Ping the socket
        let (_check, ping) = cache.check(&mut rng, &this_node, now, node);
        assert!(ping.is_some());
        
        // Fill up the cache to force eviction of ip_ping_info entry
        let ip2 = Ipv4Addr::new(192, 168, 1, 2);
        let ip3 = Ipv4Addr::new(192, 168, 1, 3);
        let socket2 = SocketAddr::V4(SocketAddrV4::new(ip2, 8000));
        let socket3 = SocketAddr::V4(SocketAddrV4::new(ip3, 8000));
        let node2 = (Keypair::new().pubkey(), socket2);
        let node3 = (Keypair::new().pubkey(), socket3);
        
        let (_, ping2) = cache.check(&mut rng, &this_node, now, node2);
        assert!(ping2.is_some());
        let (_, ping3) = cache.check(&mut rng, &this_node, now, node3);
        assert!(ping3.is_some());
        
        // The ip_ping_info entry for ip should be evicted
        assert!(cache.ip_ping_info.peek(&ip_addr).is_none());
        
        // Now receive pong for the original socket
        let pong = Pong::new(ping.as_ref().unwrap(), &remote_keypair);
        let now = now + Duration::from_millis(1);
        assert!(cache.add(&pong, socket, now));
        
        // After adding pong, ip_ping_info should be recreated with last_ping_sent as None
        let info = cache.ip_ping_info.peek(&ip_addr);
        assert!(info.is_some());
        assert!(info.unwrap().last_ping_sent.is_none(), "last_ping_sent should be None after eviction");
        assert!(info.unwrap().last_valid_pong.is_some(), "last_valid_pong should be set");
    }

    #[test]
    fn test_multiple_sockets_same_ip_scenario() {
        // Test a realistic scenario with multiple sockets on the same IP.
        let now = Instant::now();
        let mut rng = rand::rng();
        let ttl = Duration::from_millis(256);
        let delay = Duration::from_millis(10);
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 1000);
        let this_node = Keypair::new();
        let remote_keypair = Keypair::new();
        let ip = Ipv4Addr::new(192, 168, 1, 1);
        
        let socket1 = SocketAddr::V4(SocketAddrV4::new(ip, 8000));
        let socket2 = SocketAddr::V4(SocketAddrV4::new(ip, 8001));
        let socket3 = SocketAddr::V4(SocketAddrV4::new(ip, 8002));
        
        let node1 = (remote_keypair.pubkey(), socket1);
        let node2 = (remote_keypair.pubkey(), socket2);
        let node3 = (remote_keypair.pubkey(), socket3);
        
        // Ping socket1 - should succeed
        let (check, ping1) = cache.check(&mut rng, &this_node, now, node1);
        assert!(!check);
        assert!(ping1.is_some());
        
        // Try to ping socket2 immediately - should be rate limited by IP
        let (check, ping2) = cache.check(&mut rng, &this_node, now, node2);
        assert!(!check);
        assert!(ping2.is_none());
        
        // Get pong from socket1
        let pong1 = Pong::new(ping1.as_ref().unwrap(), &remote_keypair);
        let now = now + Duration::from_millis(1);
        assert!(cache.add(&pong1, socket1, now));
        
        // Now we have a valid pong from socket1. After delay, we should be able
        // to ping socket2 without IP-level rate limiting
        let now = now + delay + Duration::from_millis(1);
        let (check, ping2) = cache.check(&mut rng, &this_node, now, node2);
        assert!(!check);
        assert!(ping2.is_some(), "Should be able to ping socket2 after valid pong from socket1");
        
        // Get pong from socket2
        let pong2 = Pong::new(ping2.as_ref().unwrap(), &remote_keypair);
        let now = now + Duration::from_millis(1);
        assert!(cache.add(&pong2, socket2, now));
        
        // Now we have valid pongs from both socket1 and socket2. Should be able
        // to ping socket3 immediately (after socket2's rate limit delay)
        let now = now + delay + Duration::from_millis(1);
        let (check, ping3) = cache.check(&mut rng, &this_node, now, node3);
        assert!(!check);
        assert!(ping3.is_some(), "Should be able to ping socket3 when valid pongs exist for other sockets on same IP");
    }

    #[test]
    fn test_expired_pong_removes_ip_trust() {
        // Test that when a pong expires, IP-level rate limiting kicks back in.
        let now = Instant::now();
        let mut rng = rand::rng();
        let ttl = Duration::from_millis(256);
        let delay = Duration::from_millis(10);
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 1000);
        let this_node = Keypair::new();
        let remote_keypair = Keypair::new();
        let ip = Ipv4Addr::new(192, 168, 1, 1);
        
        let socket1 = SocketAddr::V4(SocketAddrV4::new(ip, 8000));
        let socket2 = SocketAddr::V4(SocketAddrV4::new(ip, 8001));
        
        let node1 = (remote_keypair.pubkey(), socket1);
        let node2 = (remote_keypair.pubkey(), socket2);
        
        // Ping socket1 and get pong
        let (_check, ping1) = cache.check(&mut rng, &this_node, now, node1);
        assert!(ping1.is_some());
        let pong1 = Pong::new(ping1.as_ref().unwrap(), &remote_keypair);
        let now = now + Duration::from_millis(1);
        assert!(cache.add(&pong1, socket1, now));
        
        // Should be able to ping socket2 after delay (valid pong exists)
        let now = now + delay + Duration::from_millis(1);
        let (_check, ping2) = cache.check(&mut rng, &this_node, now, node2);
        assert!(ping2.is_some());
        
        // Get pong from socket2 as well
        let pong2 = Pong::new(ping2.as_ref().unwrap(), &remote_keypair);
        let now = now + Duration::from_millis(1);
        assert!(cache.add(&pong2, socket2, now));
        
        // Expire both pongs by waiting past TTL
        let now = now + ttl + Duration::from_millis(1);
        
        // Check socket1 - should remove expired pong
        let (check, _) = cache.check(&mut rng, &this_node, now, node1);
        assert!(!check, "Pong should be expired");
        
        // Check socket2 - should also remove expired pong
        let (check, _) = cache.check(&mut rng, &this_node, now, node2);
        assert!(!check, "Pong should be expired");
        
        // Now try to ping socket2 again - should be rate limited by IP since
        // no valid pong exists anymore. Wait for socket rate limit first.
        let now = now + delay + Duration::from_millis(1);
        let (check, ping2_again) = cache.check(&mut rng, &this_node, now, node2);
        assert!(!check);
        assert!(ping2_again.is_some(), "Should be able to ping after socket rate limit");
        
        // Immediately try to ping socket1 (same IP, different port) - should be rate limited by IP
        let (_check, ping1_again) = cache.check(&mut rng, &this_node, now, node1);
        assert!(ping1_again.is_none(), "Should be rate limited by IP when no valid pong exists");
    }

    #[test]
    fn test_expired_pong_returns_check_false() {
        let mut rng = rand::rng();
        let this_node = Keypair::new();
        let remote_node_keypair = Keypair::new();
        let remote_socket = SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(rng.random(), rng.random(), rng.random(), rng.random()),
            rng.random(),
        ));
        let remote_node = (remote_node_keypair.pubkey(), remote_socket);
        let ttl = Duration::from_secs(20 * 60); // 20 minutes
        let delay = ttl / 64;
        let mut now = Instant::now();
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 1000);

        // Add a pong for the remote node
        cache.mock_pong(remote_node.1, now);

        // Verify the pong is valid. `check` should return true
        let (check, ping) = cache.check(&mut rng, &this_node, now, remote_node);
        assert!(check, "Pong should be valid immediately after adding");
        assert!(ping.is_none(), "Should not generate ping for recent pong");

        // Advance time past TTL to expire the pong
        now = now + ttl + Duration::from_secs(1);

        // After expiration, check should return false but should_ping should be true (to re-verify)
        let (check, ping) = cache.check(&mut rng, &this_node, now, remote_node);
        assert!(!check, "Expired pong should return check=false");
        assert!(
            ping.is_some(),
            "Should generate ping to re-verify expired node"
        );
    }
}
