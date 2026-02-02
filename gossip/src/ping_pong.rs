use {
    crate::cluster_info_metrics::should_report_message_signature,
    lru::LruCache,
    rand::{CryptoRng, Rng},
    serde::{Deserialize, Serialize},
    serde_big_array::BigArray,
    siphasher::sip::SipHasher24,
    solana_hash::Hash,
    solana_keypair::{Keypair, signable::Signable},
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
    // Timestamp of last ping message sent to a remote IP.
    // Used to rate limit pings to remote addresses.
    pings: LruCache<IpAddr, Instant>,
    // Verified pong responses from remote nodes.
    pongs: LruCache<PingCacheKey, Instant>,
    // Timestamp of last ping message sent to a remote address key.
    ping_times: LruCache<PingCacheKey, Instant>,
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
enum PingCacheKey {
    Ip(Pubkey, IpAddr),
    Socket(Pubkey, SocketAddr), // For loopback (local cluster tests)
}

impl PingCacheKey {
    fn from(pubkey: Pubkey, socket: SocketAddr) -> Self {
        if socket.ip().is_loopback() {
            Self::Socket(pubkey, socket)
        } else {
            Self::Ip(pubkey, socket.ip())
        }
    }

    fn ip(self) -> IpAddr {
        match self {
            Self::Ip(_, ip) => ip,
            Self::Socket(_, socket) => socket.ip(),
        }
    }
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

    /// Checks if the pong hash, pubkey, and address match a ping message sent
    /// out previously. If so records current timestamp for the remote address and
    /// returns true.
    /// Note: Does not verify the signature.
    pub fn add(&mut self, pong: &Pong, socket: SocketAddr, now: Instant) -> bool {
        let remote_node = PingCacheKey::from(pong.pubkey(), socket);
        if !self.hashers.iter().copied().any(|hasher| {
            let token = make_ping_token::<N>(hasher, &remote_node);
            hash_ping_token(&token) == pong.hash
        }) {
            return false;
        };
        self.pongs.put(remote_node, now);
        if let Some(sent_time) = self.ping_times.pop(&remote_node)
            && should_report_message_signature(
                pong.signature(),
                PONG_SIGNATURE_SAMPLE_LEADING_ZEROS,
            )
        {
            let rtt = now.saturating_duration_since(sent_time);
            datapoint_info!(
                "ping_rtt",
                ("peer_ip", remote_node.ip().to_string(), String),
                ("rtt_us", rtt.as_micros() as i64, i64),
            );
        }
        true
    }

    /// Checks if the remote node has been pinged recently. If not, calls the
    /// given function to generates a new ping message, records current
    /// timestamp and hash of ping token, and returns the ping message.
    fn maybe_ping<R: Rng + CryptoRng>(
        &mut self,
        rng: &mut R,
        keypair: &Keypair,
        now: Instant,
        remote_pubkey: Pubkey,
        remote_socket: SocketAddr,
    ) -> Option<Ping<N>> {
        let remote_node = PingCacheKey::from(remote_pubkey, remote_socket);
        // Rate limit consecutive pings sent to a remote node (non-loopback addresses).
        if !remote_socket.ip().is_loopback() {
            if let Some(&last_ping) = self.pings.peek(&remote_socket.ip())
                && now.saturating_duration_since(last_ping) < self.rate_limit_delay
            {
                return None;
            }
            self.pings.put(remote_socket.ip(), now);
        }
        self.maybe_refresh_key(rng, now);
        let token = make_ping_token::<N>(self.hashers[0], &remote_node);
        self.ping_times.put(remote_node, now);
        Some(Ping::new(token, keypair))
    }

    /// Returns true if the remote pubkey at the address has responded to a ping message.
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
        remote_pubkey: &Pubkey,
        remote_socket: SocketAddr,
    ) -> (bool, Option<Ping<N>>) {
        let remote_pubkey = *remote_pubkey;
        let remote_node = PingCacheKey::from(remote_pubkey, remote_socket);
        let (check, should_ping) = match self.pongs.get(&remote_node) {
            None => (false, true),
            Some(t) => {
                let age = now.saturating_duration_since(*t);
                // Pop if the pong message has expired.
                if age > self.ttl {
                    self.pongs.pop(&remote_node);
                    (false, true)
                } else {
                    // If the pong message is not too recent, generate a new ping
                    // message to extend remote node verification.
                    (true, age > self.ttl / 4)
                }
            }
        };
        let ping = should_ping
            .then(|| self.maybe_ping(rng, keypair, now, remote_pubkey, remote_socket))
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
    pub fn mock_pong(&mut self, pubkey: Pubkey, socket: SocketAddr, now: Instant) {
        self.pongs.put(PingCacheKey::from(pubkey, socket), now);
    }
}

fn make_ping_token<const N: usize>(mut hasher: SipHasher24, remote_node: &PingCacheKey) -> [u8; N] {
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
        let sockets: Vec<_> = (1u8..=3)
            .map(|i| {
                SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(i, i, i, i),
                    8000 + i as u16,
                ))
            })
            .collect();
        let keypairs: Vec<_> = repeat_with(Keypair::new).take(sockets.len()).collect();
        let remote_nodes: Vec<(&Keypair, SocketAddr)> = (0..sockets.len() * 2)
            .map(|i| (&keypairs[i % sockets.len()], sockets[i % sockets.len()]))
            .collect();

        // Initially all checks should fail. The first observation of each node
        // should create a ping packet.
        let mut seen_nodes = HashSet::<PingCacheKey>::new();
        let pings: Vec<Option<Ping<32>>> = remote_nodes
            .iter()
            .map(|(keypair, socket)| {
                let node = PingCacheKey::from(keypair.pubkey(), *socket);
                let (check, ping) =
                    cache.check(&mut rng, &this_node, now, &keypair.pubkey(), *socket);
                assert!(!check);
                assert_eq!(seen_nodes.insert(node), ping.is_some());
                ping
            })
            .collect();

        let now = now + Duration::from_millis(1);
        let mut rate_limited_duplicates_checked = 0;
        for ((keypair, socket), ping) in remote_nodes.iter().zip(&pings) {
            match ping {
                None => {
                    // Rate-limited: same node observed twice in quick succession,
                    // no new ping was generated. Node has pong from first observation.
                    rate_limited_duplicates_checked += 1;
                    let (check, ping) =
                        cache.check(&mut rng, &this_node, now, &keypair.pubkey(), *socket);
                    assert!(check);
                    assert!(ping.is_none());
                }
                Some(ping) => {
                    let pong = Pong::new(ping, keypair);
                    assert!(cache.add(&pong, *socket, now));
                }
            }
        }
        assert!(
            rate_limited_duplicates_checked > 0,
            "Test must exercise rate-limited duplicate path (need duplicates in remote_nodes)"
        );

        let now = now + Duration::from_millis(1);
        // All nodes now have a recent pong packet.
        for (keypair, socket) in &remote_nodes {
            let (check, ping) = cache.check(&mut rng, &this_node, now, &keypair.pubkey(), *socket);
            assert!(check);
            assert!(ping.is_none());
        }

        let now = now + ttl / 4 + Duration::from_millis(1);
        // All nodes still have a valid pong packet, but the cache will create
        // a new ping packet to extend verification.
        seen_nodes.clear();
        for (keypair, socket) in &remote_nodes {
            let node = PingCacheKey::from(keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, &keypair.pubkey(), *socket);
            assert!(check);
            assert_eq!(seen_nodes.insert(node), ping.is_some());
        }

        let now = now + Duration::from_millis(1);
        // All nodes still have a valid pong packet, and a very recent ping
        // packet pending response. So no new ping packet will be created.
        for (keypair, socket) in &remote_nodes {
            let (check, ping) = cache.check(&mut rng, &this_node, now, &keypair.pubkey(), *socket);
            assert!(check);
            assert!(ping.is_none());
        }

        let now = now + ttl;
        // Pong packets have expired. The first observation of each node will
        // remove the expired pong packet from cache and create a new ping packet.
        // Duplicates are rate-limited (no new ping).
        seen_nodes.clear();
        let mut expired_duplicates_checked = 0;
        for (keypair, socket) in &remote_nodes {
            let node = PingCacheKey::from(keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, &keypair.pubkey(), *socket);
            if seen_nodes.insert(node) {
                assert!(!check, "Expired pong should return check=false");
                assert!(
                    ping.is_some(),
                    "Should generate ping to re-verify expired node"
                );
            } else {
                expired_duplicates_checked += 1;
                assert!(!check);
                assert!(ping.is_none());
            }
        }
        assert!(
            expired_duplicates_checked > 0,
            "Test must exercise expired-duplicate path (need duplicates in remote_nodes)"
        );

        let now = now + Duration::from_millis(1);
        // No valid pong packet in the cache. A recent ping packet already
        // created, so no new one will be created.
        for (keypair, socket) in &remote_nodes {
            let (check, ping) = cache.check(&mut rng, &this_node, now, &keypair.pubkey(), *socket);
            assert!(!check);
            assert!(ping.is_none());
        }

        let now = now + ttl / 64;
        // No valid pong packet in the cache. Another ping packet will be
        // created for the first observation of each node.
        seen_nodes.clear();
        for (keypair, socket) in &remote_nodes {
            let node = PingCacheKey::from(keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, &keypair.pubkey(), *socket);
            assert!(!check);
            assert_eq!(seen_nodes.insert(node), ping.is_some());
        }
    }

    #[test]
    fn test_expired_pong_returns_check_false() {
        let mut rng = rand::rng();
        let this_node = Keypair::new();
        let remote_socket = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 10, 10, 10), 8000));
        let ttl = Duration::from_secs(20 * 60); // 20 minutes
        let delay = ttl / 64;
        let mut now = Instant::now();
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 1000);

        // Add a pong for the remote node
        cache.mock_pong(this_node.pubkey(), remote_socket, now);

        // Verify the pong is valid. `check` should return true
        let (check, ping) = cache.check(
            &mut rng,
            &this_node,
            now,
            &this_node.pubkey(),
            remote_socket,
        );
        assert!(check, "Pong should be valid immediately after adding");
        assert!(ping.is_none(), "Should not generate ping for recent pong");

        // Advance time past TTL to expire the pong
        now = now + ttl + Duration::from_secs(1);

        // After expiration, check should return false but should_ping should be true (to re-verify)
        let (check, ping) = cache.check(
            &mut rng,
            &this_node,
            now,
            &this_node.pubkey(),
            remote_socket,
        );
        assert!(!check, "Expired pong should return check=false");
        assert!(
            ping.is_some(),
            "Should generate ping to re-verify expired node"
        );
    }

    #[test]
    fn test_ping_cache_requires_pubkey_for_same_ip() {
        let mut rng = rand::rng();
        let ttl = Duration::from_secs(60);
        let delay = Duration::from_secs(1);
        let mut now = Instant::now();
        let mut cache = PingCache::<32>::new(&mut rng, now, ttl, delay, /*cap=*/ 1000);
        let this_node = Keypair::new();
        let remote_a = Keypair::new();
        let remote_b = Keypair::new();
        let socket_a = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), 8000));
        let socket_b = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), 8001));

        let (check, ping) = cache.check(&mut rng, &this_node, now, &remote_a.pubkey(), socket_a);
        assert!(!check);
        let ping_a = ping.expect("expected ping for first observation");
        let pong_a = Pong::new(&ping_a, &remote_a);
        assert!(cache.add(&pong_a, socket_a, now));

        now = now + delay + Duration::from_millis(1);
        let (check, ping) = cache.check(&mut rng, &this_node, now, &remote_b.pubkey(), socket_b);
        assert!(
            !check,
            "Different pubkey on same IP should not be verified by prior pong"
        );
        let ping_b = ping.expect("expected ping for unverified pubkey");
        let pong_b = Pong::new(&ping_b, &remote_b);
        assert!(cache.add(&pong_b, socket_b, now));

        let now = now + Duration::from_millis(1);
        let (check, ping) = cache.check(&mut rng, &this_node, now, &remote_b.pubkey(), socket_b);
        assert!(check);
        assert!(ping.is_none());
    }
}
