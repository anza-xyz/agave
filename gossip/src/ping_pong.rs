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
    // Timestamp of last ping message sent to a remote node.
    // Used to rate limit pings to remote nodes.
    pings: LruCache<(Pubkey, SocketAddr), Instant>,
    // Verified pong responses from remote nodes.
    pongs: LruCache<(Pubkey, SocketAddr), Instant>,
    // Timestamp of last ping message sent to a remote IP.
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
            hashers: std::array::from_fn(|_| SipHasher24::new_with_key(&rng.gen())),
            key_refresh: now,
            pings: LruCache::new(cap),
            pongs: LruCache::new(cap),
            ping_times: LruCache::new(cap),
        }
    }

    /// Checks if the pong hash, pubkey and socket match a ping message sent
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
        self.pongs.put(remote_node, now);
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

    /// Checks if the remote node has been pinged recently. If not, calls the
    /// given function to generates a new ping message, records current
    /// timestamp and hash of ping token, and returns the ping message.
    fn maybe_ping<R: Rng + CryptoRng>(
        &mut self,
        rng: &mut R,
        keypair: &Keypair,
        now: Instant,
        remote_node: (Pubkey, SocketAddr),
    ) -> Option<Ping<N>> {
        // Rate limit consecutive pings sent to a remote node.
        if matches!(self.pings.peek(&remote_node),
            Some(&t) if now.saturating_duration_since(t) < self.rate_limit_delay)
        {
            return None;
        }
        self.pings.put(remote_node, now);
        self.maybe_refresh_key(rng, now);
        let token = make_ping_token::<N>(self.hashers[0], &remote_node);
        self.ping_times.put(remote_node.1.ip(), Instant::now());
        Some(Ping::new(token, keypair))
    }

    /// Returns true if the remote node has responded to a ping message.
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
        let (check, should_ping) = match self.pongs.get(&remote_node) {
            None => (false, true),
            Some(t) => {
                let age = now.saturating_duration_since(*t);
                // Pop if the pong message has expired.
                if age > self.ttl {
                    self.pongs.pop(&remote_node);
                }
                // If the pong message is not too recent, generate a new ping
                // message to extend remote node verification.
                (true, age > self.ttl / 8)
            }
        };
        let ping = should_ping
            .then(|| self.maybe_ping(rng, keypair, now, remote_node))
            .flatten();
        (check, ping)
    }

    fn maybe_refresh_key<R: Rng + CryptoRng>(&mut self, rng: &mut R, now: Instant) {
        if now.checked_duration_since(self.key_refresh) > Some(KEY_REFRESH_CADENCE) {
            let hasher = SipHasher24::new_with_key(&rng.gen());
            self.hashers[1] = std::mem::replace(&mut self.hashers[0], hasher);
            self.key_refresh = now;
        }
    }

    /// Only for tests and simulations.
    pub fn mock_pong(&mut self, node: Pubkey, socket: SocketAddr, now: Instant) {
        self.pongs.put((node, socket), now);
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
        let mut rng = rand::thread_rng();
        let keypair = Keypair::new();
        let ping = Ping::<32>::new(rng.gen(), &keypair);
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
        let mut rng = rand::thread_rng();
        let ttl = Duration::from_millis(256);
        let delay = ttl / 64;
        let mut cache = PingCache::new(&mut rng, Instant::now(), ttl, delay, /*cap=*/ 1000);
        let this_node = Keypair::new();
        let keypairs: Vec<_> = repeat_with(Keypair::new).take(8).collect();
        let sockets: Vec<_> = repeat_with(|| {
            SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(rng.gen(), rng.gen(), rng.gen(), rng.gen()),
                rng.gen(),
            ))
        })
        .take(8)
        .collect();
        let remote_nodes: Vec<(&Keypair, SocketAddr)> = repeat_with(|| {
            let keypair = &keypairs[rng.gen_range(0..keypairs.len())];
            let socket = sockets[rng.gen_range(0..sockets.len())];
            (keypair, socket)
        })
        .take(128)
        .collect();

        // Initially all checks should fail. The first observation of each node
        // should create a ping packet.
        let mut seen_nodes = HashSet::<(Pubkey, SocketAddr)>::new();
        let pings: Vec<Option<Ping<32>>> = remote_nodes
            .iter()
            .map(|(keypair, socket)| {
                let node = (keypair.pubkey(), *socket);
                let (check, ping) = cache.check(&mut rng, &this_node, now, node);
                assert!(!check);
                assert_eq!(seen_nodes.insert(node), ping.is_some());
                ping
            })
            .collect();

        let now = now + Duration::from_millis(1);
        for ((keypair, socket), ping) in remote_nodes.iter().zip(&pings) {
            match ping {
                None => {
                    // Already have a recent ping packets for nodes, so no new
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
        // All nodes now have a recent pong packet.
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(check);
            assert!(ping.is_none());
        }

        let now = now + ttl / 8;
        // All nodes still have a valid pong packet, but the cache will create
        // a new ping packet to extend verification.
        seen_nodes.clear();
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(check);
            assert_eq!(seen_nodes.insert(node), ping.is_some());
        }

        let now = now + Duration::from_millis(1);
        // All nodes still have a valid pong packet, and a very recent ping
        // packet pending response. So no new ping packet will be created.
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(check);
            assert!(ping.is_none());
        }

        let now = now + ttl;
        // Pong packets are still valid but expired. The first observation of
        // each node will remove the pong packet from cache and create a new
        // ping packet.
        seen_nodes.clear();
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            if seen_nodes.insert(node) {
                assert!(check);
                assert!(ping.is_some());
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
        // created for the first observation of each node.
        seen_nodes.clear();
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(!check);
            assert_eq!(seen_nodes.insert(node), ping.is_some());
        }
    }
}
