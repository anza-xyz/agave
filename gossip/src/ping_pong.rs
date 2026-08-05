use {
    crate::cluster_info_metrics::should_report_message_signature,
    indexmap::IndexMap,
    lazy_lru::LruCache,
    rand::{CryptoRng, Rng},
    serde::{Deserialize, Serialize},
    serde_big_array::BigArray,
    solana_hash::Hash,
    solana_keypair::{Keypair, signable::Signable},
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    solana_signature::Signature,
    solana_signer::Signer,
    std::{
        borrow::Cow,
        net::{IpAddr, SocketAddr},
        ops::Range,
        time::{Duration, Instant},
    },
    wincode::{SchemaRead, SchemaWrite},
};

const PING_PONG_HASH_PREFIX: &[u8] = "SOLANA_PING_PONG".as_bytes();
const PONG_SIGNATURE_SAMPLE_LEADING_ZEROS: u32 = 5;

// For backward compatibility we are using a const generic parameter here.
// N should always be >= 8 and only the first 8 bytes are used. So the new code
// should only use N == 8.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[derive(Debug, Deserialize, PartialEq, Serialize, SchemaRead, SchemaWrite)]
pub struct Ping<const N: usize> {
    from: Pubkey,
    #[serde(with = "BigArray")]
    token: [u8; N],
    signature: Signature,
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "Gab1D5ug6ZAB5sRNmBpoM8JyxsixccLLaWxYZwmueVYA",
        abi_serializer = ["bincode", "wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Debug, Deserialize, PartialEq, Serialize, SchemaRead, SchemaWrite)]
// repr(C) makes this struct zero-copy eligible in wincode.
#[repr(C)]
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
    // Timeout range (ms) waiting for a Pong. Randomized per-entry to stagger expiry.
    outstanding_ping_timeout_ms: Range<u64>,
    // Capacity for the pings store.
    max_pings: usize,
    // Expiry time and expected pong hash for each pinged remote node.
    pings: IndexMap<(Pubkey, SocketAddr), (Instant, Hash)>,
    // Verified pong responses from remote nodes.
    pongs: LruCache<(Pubkey, SocketAddr), Instant>,
    // Timestamp of last ping message sent to a remote IP.
    ping_times: LruCache<IpAddr, Instant>,
}

/// max number of slots in [`PingCache::pings`] to probe when looking for a
/// reclaimable entry for a new ping. Probing only happens once the cache is
/// full. The chance of hitting at least one timed-out (evictable) slot is
/// `1 - (1 - f)^MAX_PING_PROBES`, where `f` is the fraction of entries that
/// have timed out. E.g. with `f = 0.5` that is `1 - 0.5^8` ~ 99.6%.
const MAX_PING_PROBES: usize = 8;

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
    pub fn new(ttl: Duration, outstanding_ping_timeout_ms: Range<u64>, max_pings: usize) -> Self {
        assert!(
            outstanding_ping_timeout_ms.start < outstanding_ping_timeout_ms.end,
            "outstanding_ping_timeout_ms must be non-empty"
        );
        assert!(
            outstanding_ping_timeout_ms.end <= (ttl / 2).as_millis() as u64,
            "outstanding_ping_timeout_ms.end must be <= ttl/2"
        );
        assert!(max_pings > 0, "Must cache nonzero amount of hosts");
        Self {
            ttl,
            outstanding_ping_timeout_ms,
            max_pings,
            pings: IndexMap::with_capacity(max_pings),
            pongs: LruCache::new(max_pings),
            ping_times: LruCache::new(max_pings),
        }
    }

    /// Checks if the pong hash matches a ping message sent out previously.
    /// If so records current timestamp for the remote node and returns true.
    /// Note: Does not verify the signature.
    pub fn add(&mut self, pong: &Pong, socket: SocketAddr, now: Instant) -> bool {
        let remote_node = (pong.pubkey(), socket);
        // We can not just pop an entry from self.pings based on remote_node
        // contents - that value is attacker controlled and could invalidate an
        // in-flight ping.
        let Some((index, _, (_timeout, hash))) = self.pings.get_full(&remote_node) else {
            return false;
        };
        // check only hash, a late Pong is still perfectly valid.
        if *hash != pong.hash {
            return false;
        }
        // at this point we are certain the pong is valid.
        self.pings.swap_remove_index(index);
        self.pongs.put(remote_node, now);
        if let Some(sent_time) = self.ping_times.pop(&socket.ip())
            && should_report_message_signature(
                pong.signature(),
                PONG_SIGNATURE_SAMPLE_LEADING_ZEROS,
            )
        {
            let rtt = now.saturating_duration_since(sent_time);
            datapoint_info!(
                "ping_rtt",
                ("peer_ip", socket.ip().to_string(), String),
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
        remote_node: (Pubkey, SocketAddr),
    ) -> Option<Ping<N>> {
        // If the existing ping is still in-flight don't send another one.
        let is_new_key = if let Some((expiry, _)) = self.pings.get(&remote_node) {
            if now < *expiry {
                return None;
            }
            false // existing entry will be updated in-place
        } else {
            true // no entry for this node yet
        };

        // If this is a new entry and the pings store is at capacity,
        // probe random existing entries and evict the first timed-out one
        // (expiry in the past, peer never responded).
        // Decline if all probes are in-flight — avoids evicting challenges
        // still awaiting a Pong.
        if is_new_key && self.pings.len() >= self.max_pings {
            let n = self.pings.len();
            let mut evicted = false;
            for _ in 0..MAX_PING_PROBES {
                let idx = rng.random_range(0..n);
                if let Some((_, (expiry, _))) = self.pings.get_index(idx)
                    && now >= *expiry
                {
                    self.pings.swap_remove_index(idx);
                    evicted = true;
                    break;
                }
            }
            if !evicted {
                return None;
            }
        }

        let token = {
            let mut token = [0u8; N];
            const FILL: usize = std::mem::size_of::<u64>();
            const { assert!(N >= FILL, "N must be >= size_of::<u64>()") };
            let entropy: [u8; FILL] = rng.random();
            *token
                .first_chunk_mut::<FILL>()
                .expect("token is known to fit FILL bytes") = entropy;
            token
        };
        // Deadline by which we expect a reply. Randomized to stagger expiries across entries.
        let expiry = now
            + Duration::from_millis(rng.random_range(
                self.outstanding_ping_timeout_ms.start..self.outstanding_ping_timeout_ms.end,
            ));
        // The hash we expect to see in the Pong message
        let ping_hash = hash_ping_token(&token);
        self.pings.insert(remote_node, (expiry, ping_hash));
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
                    (false, true)
                } else {
                    // If the pong message is not too recent, generate a new ping
                    // message to extend remote node verification.
                    (true, age > self.ttl / 4)
                }
            }
        };
        let ping = should_ping
            .then(|| self.maybe_ping(rng, keypair, now, remote_node))
            .flatten();
        (check, ping)
    }

    /// Only for tests and simulations.
    pub fn mock_pong(&mut self, node: Pubkey, socket: SocketAddr, now: Instant) {
        self.pongs.put((node, socket), now);
    }
}

fn hash_ping_token<const N: usize>(token: &[u8; N]) -> Hash {
    solana_sha256_hasher::hashv(&[PING_PONG_HASH_PREFIX, token])
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::cluster_info::{
            GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS, GOSSIP_PING_CACHE_TTL,
        },
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
        let delay_ms = delay.as_millis() as u64;
        let mut cache = PingCache::new(ttl, delay_ms..delay_ms + 1, /*cap=*/ 1000);
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

        let now = now + ttl / 4 + Duration::from_millis(1);
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
        // created for the first observation of each node.
        seen_nodes.clear();
        for (keypair, socket) in &remote_nodes {
            let node = (keypair.pubkey(), *socket);
            let (check, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(!check);
            assert_eq!(seen_nodes.insert(node), ping.is_some());
        }
    }

    #[test]
    fn test_wincode_compatibility_ping() {
        let mut rng = rand::rng();
        for _ in 0..1000 {
            let keypair = Keypair::new();
            let ping = Ping::<32>::new(rng.random(), &keypair);

            let bincode_bytes = bincode::serialize(&ping).unwrap();
            let wincode_decoded: Ping<32> = wincode::deserialize(&bincode_bytes).unwrap();
            assert_eq!(ping, wincode_decoded);

            let wincode_bytes = wincode::serialize(&ping).unwrap();
            let bincode_decoded: Ping<32> = bincode::deserialize(&wincode_bytes).unwrap();
            assert_eq!(ping, bincode_decoded);

            assert_eq!(bincode_bytes, wincode_bytes);
        }
    }

    #[test]
    fn test_wincode_compatibility_pong() {
        let mut rng = rand::rng();
        for _ in 0..1000 {
            let keypair = Keypair::new();
            let ping = Ping::<32>::new(rng.random(), &keypair);
            let pong = Pong::new(&ping, &keypair);

            let bincode_bytes = bincode::serialize(&pong).unwrap();
            let wincode_decoded: Pong = wincode::deserialize(&bincode_bytes).unwrap();
            assert_eq!(pong, wincode_decoded);

            let wincode_bytes = wincode::serialize(&pong).unwrap();
            let bincode_decoded: Pong = bincode::deserialize(&wincode_bytes).unwrap();
            assert_eq!(pong, bincode_decoded);

            assert_eq!(bincode_bytes, wincode_bytes);
        }
    }

    #[test]
    fn test_ping_cache_full_no_stale() {
        // Verify that when the pings cache is at capacity and all entries are
        // fresh, new pings are declined rather than evicting in-flight ones.
        let mut rng = rand::rng();
        let this_node = Keypair::new();
        let cap = 3usize;
        let mut cache = PingCache::<32>::new(
            GOSSIP_PING_CACHE_TTL,
            GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS,
            cap,
        );
        let sockets: Vec<SocketAddr> = (1u8..=4)
            .map(|i| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(i, i, i, i), 8000)))
            .collect();
        let keypairs: Vec<Keypair> = (0..4).map(|_| Keypair::new()).collect();

        // Fill cache to capacity (3 entries)
        let mut pings = Vec::new();
        for i in 0..cap {
            let node = (keypairs[i].pubkey(), sockets[i]);
            let (check, ping) = cache.check(&mut rng, &this_node, Instant::now(), node);
            assert!(!check, "No pong yet, check should be false");
            assert!(ping.is_some(), "Should issue ping for entry {i}");
            pings.push((i, ping.unwrap()));
        }
        assert_eq!(cache.pings.len(), cap, "Cache should be at capacity");

        // 4th new node must be declined — cache full, no stale entries
        let node4 = (keypairs[3].pubkey(), sockets[3]);
        let (check, ping) = cache.check(&mut rng, &this_node, Instant::now(), node4);
        assert!(!check, "No pong, check should be false");
        assert!(
            ping.is_none(),
            "Must decline new ping when cache is full and no stale entries"
        );

        // Complete handshake for node 0 — frees one slot
        let (idx0, ping0) = &pings[0];
        let pong0 = Pong::new(ping0, &keypairs[*idx0]);
        assert!(
            cache.add(&pong0, sockets[0], Instant::now()),
            "Valid pong should be accepted"
        );
        assert_eq!(
            cache.pings.len(),
            cap - 1,
            "One slot should have been freed"
        );

        // Now 4th node should get a ping
        let (check, ping) = cache.check(&mut rng, &this_node, Instant::now(), node4);
        assert!(!check, "No pong for node4 yet");
        assert!(
            ping.is_some(),
            "Should issue ping for node4 after slot freed"
        );
    }

    #[test]
    fn test_ping_cache_full_with_stale() {
        // Verify that when the pings cache is at capacity and entries are timed
        // out (age >= outstanding_ping_timeout, peer never responded), a new
        // ping reclaims a stale slot.
        let mut rng = rand::rng();
        let this_node = Keypair::new();
        let cap = 3usize;
        let mut cache = PingCache::<32>::new(
            GOSSIP_PING_CACHE_TTL,
            GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS,
            cap,
        );
        let sockets: Vec<SocketAddr> = (1u8..=4)
            .map(|i| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(i, i, i, i), 8000)))
            .collect();
        let keypairs: Vec<Keypair> = (0..4).map(|_| Keypair::new()).collect();
        let now = Instant::now();

        // Fill cache to capacity with nodes that won't answer pongs
        for i in 0..cap {
            let node = (keypairs[i].pubkey(), sockets[i]);
            let (_, ping) = cache.check(&mut rng, &this_node, now, node);
            assert!(ping.is_some(), "Should issue ping for entry {i}");
        }
        assert_eq!(cache.pings.len(), cap, "Cache should be at capacity");

        // Advance time so all in-flight pings are now stale
        let expired =
            now + Duration::from_millis(GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS.end + 1);

        // 4th node should get a ping by reclaiming a stale slot
        let node4 = (keypairs[3].pubkey(), sockets[3]);
        // The 8-probe eviction is normally probabilistic; but with all entries
        // stale, we expect it to always succeed.
        let (check, ping) = cache.check(&mut rng, &this_node, expired, node4);
        assert!(!check, "No pong for node4");
        assert!(
            ping.is_some(),
            "Should issue ping for node4 by reclaiming a stale entry"
        );
        assert_eq!(
            cache.pings.len(),
            cap,
            "Net size unchanged: one evicted, one inserted"
        );
    }

    #[test]
    fn test_expired_pong_returns_check_false() {
        let mut rng = rand::rng();
        let this_node = Keypair::new();
        let remote_socket = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 10, 10, 10), 8000));
        let remote_node_keypair = Keypair::new();
        let remote_node = (remote_node_keypair.pubkey(), remote_socket);
        let mut now = Instant::now();
        let mut cache = PingCache::<32>::new(
            GOSSIP_PING_CACHE_TTL,
            GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS,
            /*cap=*/ 1000,
        );

        // Add a pong for the remote node
        cache.mock_pong(remote_node.0, remote_node.1, now);

        // Verify the pong is valid. `check` should return true
        let (check, ping) = cache.check(&mut rng, &this_node, now, remote_node);
        assert!(check, "Pong should be valid immediately after adding");
        assert!(ping.is_none(), "Should not generate ping for recent pong");

        // Advance time past TTL to expire the pong
        now = now + GOSSIP_PING_CACHE_TTL + Duration::from_secs(1);

        // After expiration, check should return false but should_ping should be true (to re-verify)
        let (check, ping) = cache.check(&mut rng, &this_node, now, remote_node);
        assert!(!check, "Expired pong should return check=false");
        assert!(
            ping.is_some(),
            "Should generate ping to re-verify expired node"
        );
    }
}
