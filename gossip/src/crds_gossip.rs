//! Crds Gossip.
//!
//! This module ties together Crds and the push and pull gossip overlays.  The interface is
//! designed to run with a simulator or over a UDP network connection with messages up to a
//! packet::PACKET_DATA_SIZE size.

use {
    crate::{
        cluster_info_metrics::GossipStats,
        crds::{Crds, GossipRoute, NodesCursor},
        crds_data::CrdsData,
        crds_gossip_error::CrdsGossipError,
        crds_gossip_pull::{
            CrdsFilter, CrdsGossipPull, CrdsTimeouts, ProcessPullStats, PullRequest,
        },
        crds_gossip_push::CrdsGossipPush,
        crds_value::{CrdsValue, CrdsValueLabel},
        duplicate_shred::{self, DuplicateShred, DuplicateShredIndex, MAX_DUPLICATE_SHREDS},
        protocol::{Ping, PingCache},
    },
    parking_lot::RwLock,
    rand::{CryptoRng, Rng},
    rayon::ThreadPool,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::{Address, Keypair},
    solana_ledger::shred::Shred,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_time_utils::timestamp,
    std::{
        cmp,
        collections::{HashMap, HashSet},
        net::SocketAddr,
        sync::Mutex,
        time::{Duration, Instant},
    },
};

#[derive(Default)]
pub struct CrdsGossip {
    pub crds: RwLock<Crds>,
    pub push: CrdsGossipPush,
    pub pull: CrdsGossipPull,
}

impl CrdsGossip {
    /// Process a push message to the network.
    ///
    /// Returns unique origins' pubkeys of upserted values.
    pub fn process_push_message(
        &self,
        messages: Vec<(/*from:*/ Pubkey, Vec<CrdsValue>)>,
        now: u64,
    ) -> HashSet<Pubkey> {
        self.push.process_push_message(&self.crds, messages, now)
    }

    /// Remove redundant paths in the network.
    pub fn prune_received_cache<I>(
        &self,
        self_pubkey: &Pubkey,
        origins: I, // Unique pubkeys of crds values' owners.
        stakes: &HashMap<Pubkey, u64>,
    ) -> HashMap</*gossip peer:*/ Pubkey, /*origins:*/ Vec<Pubkey>>
    where
        I: IntoIterator<Item = Pubkey>,
    {
        self.push.prune_received_cache(self_pubkey, origins, stakes)
    }

    pub fn new_push_messages(
        &self,
        pubkey: &Pubkey, // This node.
        now: u64,
        stakes: &HashMap<Pubkey, u64>,
        should_retain_crds_value: impl Fn(&CrdsValue) -> bool,
    ) -> (
        Vec<CrdsValue>, // unique CrdsValues pushed out to peers
        // map of pubkeys to indices in Vec<CrdsValue> pushed to that peer
        HashMap<Pubkey, Vec</*index:*/ usize>>,
        usize, // number of push messages
    ) {
        self.push
            .new_push_messages(pubkey, &self.crds, now, stakes, should_retain_crds_value)
    }

    pub(crate) fn push_duplicate_shred<F>(
        &self,
        keypair: &Keypair,
        shred: &Shred,
        other_payload: &[u8],
        leader_schedule: Option<F>,
        // Maximum serialized size of each DuplicateShred chunk payload.
        max_payload_size: usize,
        shred_version: u16,
    ) -> Result<(), duplicate_shred::Error>
    where
        F: FnOnce(Slot) -> Option<Pubkey>,
    {
        let pubkey = keypair.pubkey();
        let slot = shred.slot();
        let Some(mut offset) = get_duplicate_shred_offset(&self.crds.read(), &pubkey, slot) else {
            return Ok(());
        };
        let chunks = duplicate_shred::from_shred(
            shred.clone(),
            pubkey,
            Vec::from(other_payload),
            leader_schedule,
            timestamp(),
            max_payload_size,
            shred_version,
        )?;
        let mut entries = sign_duplicate_shreds(chunks, offset, keypair);
        let mut target_hashes = vec![None; entries.len()];
        let mut crds = loop {
            let crds = self.crds.write();
            let Some(current_offset) = get_duplicate_shred_offset(&crds, &pubkey, slot) else {
                return Ok(());
            };
            let get_hash = |label| crds.get::<&CrdsValue>(&label).map(|value| *value.hash());
            // Re-sign if the indices moved or a rejecting target changed since signing.
            if current_offset == offset
                && entries
                    .iter()
                    .zip(&target_hashes)
                    .all(|(entry, target_hash)| {
                        crds.upserts(entry) || get_hash(entry.label()) == *target_hash
                    })
            {
                break crds;
            }
            target_hashes = (0..entries.len())
                .map(|k| {
                    let index = duplicate_shred_index(current_offset, k);
                    get_hash(CrdsValueLabel::DuplicateShred(index, pubkey))
                })
                .collect();
            drop(crds);
            offset = current_offset;
            entries = resign_duplicate_shreds(entries, offset, keypair);
        };
        let now = timestamp();
        // Drain so that the buffer is freed after releasing the lock.
        let errors: Vec<_> = entries
            .drain(..)
            .filter_map(|entry| crds.insert(entry, now, GossipRoute::LocalMessage).err())
            .collect();
        drop(crds);
        for err in errors {
            error!("push_duplicate_shred failed: {err:?}");
        }
        Ok(())
    }

    /// Add the `from` to the peer's filter of nodes.
    pub fn process_prune_msg(
        &self,
        self_pubkey: &Pubkey,
        peer: &Pubkey,
        destination: &Pubkey,
        origin: &[Pubkey],
        wallclock: u64,
        now: u64,
        stakes: &HashMap<Pubkey, u64>,
    ) -> Result<(), CrdsGossipError> {
        if now > wallclock.saturating_add(self.push.prune_timeout) {
            Err(CrdsGossipError::PruneMessageTimeout)
        } else if self_pubkey == destination {
            self.push
                .process_prune_msg(self_pubkey, peer, origin, stakes);
            Ok(())
        } else {
            Err(CrdsGossipError::BadPruneDestination)
        }
    }

    /// Refresh the push active set.
    pub fn refresh_push_active_set(
        &self,
        self_keypair: &Keypair,
        self_shred_version: u16,
        stakes: &HashMap<Pubkey, u64>,
        gossip_validators: Option<&HashSet<Pubkey>>,
        ping_cache: &Mutex<PingCache>,
        pings: &mut Vec<(SocketAddr, Ping)>,
        socket_addr_space: &SocketAddrSpace,
    ) {
        self.push.refresh_push_active_set(
            &self.crds,
            stakes,
            gossip_validators,
            self_keypair,
            self_shred_version,
            ping_cache,
            pings,
            socket_addr_space,
        )
    }

    /// Generate a random request.
    #[allow(clippy::too_many_arguments)]
    pub fn new_pull_request(
        &self,
        thread_pool: &ThreadPool,
        self_keypair: &Keypair,
        self_shred_version: u16,
        now: u64,
        gossip_validators: Option<&HashSet<Pubkey>>,
        stakes: &HashMap<Pubkey, u64>,
        bloom_size: usize,
        ping_cache: &Mutex<PingCache>,
        pings: &mut Vec<(SocketAddr, Ping)>,
        socket_addr_space: &SocketAddrSpace,
    ) -> Result<impl Iterator<Item = (SocketAddr, CrdsFilter)> + Clone + use<>, CrdsGossipError>
    {
        self.pull.new_pull_request(
            thread_pool,
            &self.crds,
            self_keypair,
            self_shred_version,
            now,
            gossip_validators,
            stakes,
            bloom_size,
            ping_cache,
            pings,
            socket_addr_space,
        )
    }

    pub fn generate_pull_responses(
        &self,
        requests: &[PullRequest],
        output_size_limit: usize, // Limit number of crds values returned.
        now: u64,
        should_retain_crds_value: impl Fn(&CrdsValue) -> bool,
        try_consume_scan_budget: impl Fn(&PullRequest, usize) -> bool,
        stats: &GossipStats,
    ) -> Vec<Vec<CrdsValue>> {
        CrdsGossipPull::generate_pull_responses(
            &self.crds,
            requests,
            output_size_limit,
            now,
            should_retain_crds_value,
            try_consume_scan_budget,
            stats,
        )
    }

    pub fn filter_pull_responses(
        &self,
        timeouts: &CrdsTimeouts,
        response: Vec<CrdsValue>,
        now: u64,
        process_pull_stats: &mut ProcessPullStats,
    ) -> (
        Vec<CrdsValue>, // valid responses.
        Vec<CrdsValue>, // responses with expired timestamps.
        Vec<Hash>,      // hash of outdated values.
    ) {
        self.pull
            .filter_pull_responses(&self.crds, timeouts, response, now, process_pull_stats)
    }

    /// Process a pull response.
    pub fn process_pull_responses(
        &self,
        responses: Vec<CrdsValue>,
        responses_expired_timeout: Vec<CrdsValue>,
        failed_inserts: Vec<Hash>,
        now: u64,
        process_pull_stats: &mut ProcessPullStats,
    ) {
        self.pull.process_pull_responses(
            &self.crds,
            responses,
            responses_expired_timeout,
            failed_inserts,
            now,
            process_pull_stats,
        );
    }

    pub fn make_timeouts<'a>(
        &self,
        self_pubkey: Pubkey,
        stakes: &'a HashMap<Pubkey, u64>,
        purge_duration: Duration,
    ) -> CrdsTimeouts<'a> {
        self.pull.make_timeouts(self_pubkey, stakes, purge_duration)
    }

    pub fn purge(
        &self,
        self_pubkey: &Pubkey,
        thread_pool: &ThreadPool,
        now: u64,
        timeouts: &CrdsTimeouts,
    ) -> usize {
        debug_assert_eq!(timeouts[self_pubkey], u64::MAX);
        debug_assert_ne!(timeouts[&Pubkey::default()], 0u64);
        let rv = CrdsGossipPull::purge_active(thread_pool, &self.crds, now, timeouts);
        self.crds
            .write()
            .trim_purged(now.saturating_sub(5 * self.pull.crds_timeout));
        self.pull.purge_failed_inserts(now);
        rv
    }
}

pub(crate) type GossipStakePubkey = (
    /* gossip */ SocketAddr,
    /* stake */ u64,
    /* node pubkey */ Address,
);

// Returns active and valid cluster nodes to gossip with.
/// Returns the first index for a new proof of the slot, or None if one exists.
/// New proofs are appended until all indices are used, then overwrite the oldest.
fn get_duplicate_shred_offset(
    crds: &Crds,
    pubkey: &Pubkey,
    slot: Slot,
) -> Option<DuplicateShredIndex> {
    let mut num_dup_shreds = 0;
    let mut oldest = None;
    for value in crds.get_records(pubkey) {
        if let CrdsData::DuplicateShred(ix, value) = value.value.data() {
            if value.slot == slot {
                return None;
            }
            num_dup_shreds += 1;
            let record = (value.wallclock, *ix);
            oldest = Some(oldest.map_or(record, |oldest| cmp::min(oldest, record)));
        }
    }
    if num_dup_shreds < MAX_DUPLICATE_SHREDS {
        Some(num_dup_shreds)
    } else {
        oldest.map(|(_, ix)| ix)
    }
}

fn duplicate_shred_index(offset: DuplicateShredIndex, k: usize) -> DuplicateShredIndex {
    (offset + k as DuplicateShredIndex) % MAX_DUPLICATE_SHREDS
}

fn sign_duplicate_shreds(
    chunks: impl IntoIterator<Item = DuplicateShred>,
    offset: DuplicateShredIndex,
    keypair: &Keypair,
) -> Vec<CrdsValue> {
    chunks
        .into_iter()
        .enumerate()
        .map(|(k, chunk)| {
            let index = duplicate_shred_index(offset, k);
            CrdsValue::new(CrdsData::DuplicateShred(index, chunk), keypair)
        })
        .collect()
}

/// Signs the chunks again at the offset with a wallclock newer than before.
fn resign_duplicate_shreds(
    entries: Vec<CrdsValue>,
    offset: DuplicateShredIndex,
    keypair: &Keypair,
) -> Vec<CrdsValue> {
    let wallclock = timestamp();
    let chunks = entries.into_iter().map(|entry| {
        let CrdsData::DuplicateShred(_, mut chunk) = entry.into_data() else {
            unreachable!("entries are constructed from duplicate-shred chunks");
        };
        chunk.wallclock = wallclock.max(chunk.wallclock + 1);
        chunk
    });
    sign_duplicate_shreds(chunks, offset, keypair)
}

pub(crate) fn get_gossip_nodes<R: Rng>(
    rng: &mut R,
    now: u64,
    pubkey: &Pubkey, // This node.
    // By default, should only push to or pull from gossip nodes with the same
    // shred-version.
    verify_shred_version: impl Fn(/*shred_version:*/ u16) -> bool,
    crds: &RwLock<Crds>,
    gossip_validators: Option<&HashSet<Pubkey>>,
    stakes: &HashMap<Pubkey, u64>,
    socket_addr_space: &SocketAddrSpace,
) -> Vec<GossipStakePubkey> {
    // Exclude nodes which have not been active for this long.
    const ACTIVE_TIMEOUT: Duration = Duration::from_secs(60);
    let active_cutoff = now.saturating_sub(ACTIVE_TIMEOUT.as_millis() as u64);
    // Copy out the needed fields one chunk of nodes per read lock, and apply the
    // other filters outside the lock.
    let nodes = NodesCursor::collect(crds, |_, value| {
        let node = value.value.contact_info()?;
        if !verify_shred_version(node.shred_version()) {
            return None;
        }
        Some((node.gossip()?, value.local_timestamp))
    });
    nodes
        .filter_map(|(node_pubkey, (gossip, local_timestamp))| {
            if !socket_addr_space.check(&gossip)
                || &node_pubkey == pubkey
                || gossip_validators.is_some_and(|nodes| !nodes.contains(&node_pubkey))
            {
                return None;
            }

            let stake = stakes.get(&node_pubkey).copied().unwrap_or_default();
            // Exclude nodes which have not been active recently.
            if local_timestamp < active_cutoff {
                // In order to mitigate eclipse attack, for staked nodes
                // continue retrying periodically.
                if stake == 0u64 || !rng.random_ratio(1, 16) {
                    return None;
                }
            }
            Some((gossip, stake, node_pubkey))
        })
        .collect()
}

// Dedups gossip addresses, keeping only the one with the highest stake.
pub(crate) fn dedup_gossip_addresses(mut nodes: Vec<GossipStakePubkey>) -> Vec<GossipStakePubkey> {
    nodes.sort_by_key(|(gossip, stake, _pubkey)| (*gossip, cmp::Reverse(*stake)));
    nodes.dedup_by_key(|(gossip, _stake, _pubkey)| *gossip);
    nodes
}

// Pings gossip addresses if needed.
// Returns nodes which have recently responded to a ping message.
#[must_use]
pub(crate) fn maybe_ping_gossip_addresses<R: Rng + CryptoRng>(
    rng: &mut R,
    mut nodes: Vec<GossipStakePubkey>,
    keypair: &Keypair,
    ping_cache: &Mutex<PingCache>,
    pings: &mut Vec<(SocketAddr, Ping)>,
) -> Vec<GossipStakePubkey> {
    let mut ping_cache = ping_cache.lock().unwrap();
    let now = Instant::now();
    nodes.retain(|(gossip, _stake, pubkey)| {
        let (check, ping) = ping_cache.check(rng, keypair, now, (*pubkey, *gossip));
        if let Some(ping) = ping {
            pings.push((*gossip, ping));
        }
        check
    });
    nodes
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::{
            cluster_info::{GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS, GOSSIP_PING_CACHE_TTL},
            contact_info::ContactInfo,
            crds::LOCK_CHUNK_SIZE,
            duplicate_shred::tests::new_rand_shred,
            protocol::DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
        },
        solana_keypair::signable::Signable,
        solana_ledger::shred::Shredder,
        solana_sha256_hasher::hash,
        solana_time_utils::timestamp,
        std::iter::repeat_with,
        test_case::test_case,
    };

    fn new_duplicate_shreds(slot: Slot, leader: &Keypair) -> (Shred, Shred) {
        let shredder = Shredder::new(slot, slot - 1, 0, 0).unwrap();
        let mut rng = rand::rng();
        (
            new_rand_shred(&mut rng, 353, &shredder, leader),
            new_rand_shred(&mut rng, 353, &shredder, leader),
        )
    }

    fn new_first_chunk(shred: &Shred, other: &Shred, from: Pubkey) -> DuplicateShred {
        duplicate_shred::from_shred(
            shred.clone(),
            from,
            other.payload().to_vec(),
            None::<fn(Slot) -> Option<Pubkey>>,
            0,
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
            0,
        )
        .expect("valid duplicate proof")
        .next()
        .expect("nonempty duplicate proof")
    }

    fn get_chunk(
        crds: &Crds,
        pubkey: Pubkey,
        ix: DuplicateShredIndex,
    ) -> (&CrdsValue, &DuplicateShred) {
        let label = CrdsValueLabel::DuplicateShred(ix, pubkey);
        let entry = crds.get::<&CrdsValue>(&label).expect("missing proof chunk");
        let CrdsData::DuplicateShred(_, chunk) = entry.data() else {
            panic!("expected duplicate-shred chunk");
        };
        (entry, chunk)
    }

    #[test_case(false; "same_slot")]
    #[test_case(true; "different_slot")]
    fn test_push_duplicate_shred_rechecks_selection(different_slot: bool) {
        let gossip = CrdsGossip::default();
        let keypair = Keypair::new();
        let leader = Keypair::new();
        let slot = 53084024;
        let (shred, other) = new_duplicate_shreds(slot, &leader);
        let racing_slot = slot + u64::from(different_slot);
        let (racing_shred, racing_other) = new_duplicate_shreds(racing_slot, &leader);
        // Publish a racing proof during validation, which must not hold CRDS.
        let leader_schedule = |requested_slot| {
            assert_eq!(requested_slot, slot, "wrong leader lookup slot");
            drop(
                gossip
                    .crds
                    .try_write()
                    .expect("CRDS locked during validation"),
            );
            gossip
                .push_duplicate_shred(
                    &keypair,
                    &racing_shred,
                    racing_other.payload(),
                    None::<fn(Slot) -> Option<Pubkey>>,
                    DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
                    0,
                )
                .expect("publish concurrent proof");
            Some(leader.pubkey())
        };
        gossip
            .push_duplicate_shred(
                &keypair,
                &shred,
                other.payload(),
                Some(leader_schedule),
                DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
                0,
            )
            .expect("publish after concurrent proof");
        let num_chunks = |shred, other| {
            DuplicateShredIndex::from(new_first_chunk(shred, other, keypair.pubkey()).num_chunks())
        };
        let racing_chunks = num_chunks(&racing_shred, &racing_other);
        let total_chunks = if different_slot {
            racing_chunks + num_chunks(&shred, &other)
        } else {
            racing_chunks
        };
        let crds = gossip.crds.read();
        assert_eq!(
            crds.get_records(&keypair.pubkey()).count(),
            usize::from(total_chunks),
            "one complete proof per distinct slot",
        );
        assert_eq!(
            crds.num_purged(),
            0,
            "concurrent records must not be replaced or rejected",
        );
        for ix in 0..total_chunks {
            let (entry, chunk) = get_chunk(&crds, keypair.pubkey(), ix);
            assert!(entry.verify(), "invalid signature at index {ix}");
            assert_eq!(
                chunk.slot,
                if ix < racing_chunks {
                    racing_slot
                } else {
                    slot
                },
                "wrong slot at index {ix}",
            );
        }
        drop(crds);
        // An already-published slot still returns before validating its input.
        gossip
            .push_duplicate_shred(
                &keypair,
                &shred,
                shred.payload(),
                Some(|_| panic!("validated an existing proof")),
                DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
                0,
            )
            .expect("existing slot must skip validation");
    }

    #[test]
    fn test_push_duplicate_shred_rechecks_offset_after_removal() {
        let gossip = CrdsGossip::default();
        let keypair = Keypair::new();
        let leader = Keypair::new();
        let (shred, other) = new_duplicate_shreds(53084024, &leader);
        let mut seed = new_first_chunk(&shred, &other, keypair.pubkey());
        let num_chunks = DuplicateShredIndex::from(seed.num_chunks());
        seed.slot += 1;
        gossip
            .crds
            .write()
            .insert(
                CrdsValue::new_unsigned(CrdsData::DuplicateShred(0, seed)),
                0,
                GossipRoute::LocalMessage,
            )
            .expect("seed index is vacant");
        let leader_schedule = |_| {
            let mut crds = gossip
                .crds
                .try_write()
                .expect("CRDS locked during validation");
            crds.remove(&CrdsValueLabel::DuplicateShred(0, keypair.pubkey()), 0);
            // The targets stay vacant, so only the offset recheck catches this.
            Some(leader.pubkey())
        };
        gossip
            .push_duplicate_shred(
                &keypair,
                &shred,
                other.payload(),
                Some(leader_schedule),
                DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
                0,
            )
            .expect("publish after offset change");
        let crds = gossip.crds.read();
        assert_eq!(
            crds.get_records(&keypair.pubkey()).count(),
            usize::from(num_chunks),
            "the new proof must start at the removed index",
        );
        for ix in 0..num_chunks {
            let (entry, chunk) = get_chunk(&crds, keypair.pubkey(), ix);
            assert_eq!(chunk.slot, shred.slot(), "wrong slot at index {ix}");
            assert_eq!(
                u16::from(chunk.chunk_index()),
                ix,
                "wrong chunk index at {ix}"
            );
            assert!(entry.verify(), "invalid signature at index {ix}");
        }
    }

    #[test_case(None, 0; "unchanged")]
    #[test_case(Some(0), 0; "newer_target")]
    #[test_case(Some(MAX_DUPLICATE_SHREDS - 1), 0; "newer_oldest")]
    #[test_case(Some(0), 60_000; "future_target")]
    fn test_push_duplicate_shred_wraps_oldest_indices(
        newer_record: Option<DuplicateShredIndex>,
        clock_offset: u64,
    ) {
        let gossip = CrdsGossip::default();
        let keypair = Keypair::new();
        let leader = Keypair::new();
        let (shred, other) = new_duplicate_shreds(53084024, &leader);
        let mut seed = new_first_chunk(&shred, &other, keypair.pubkey());
        let num_chunks = DuplicateShredIndex::from(seed.num_chunks());
        assert!(num_chunks > 1, "the proof must wrap around to index 0");
        seed.slot += 1;
        {
            let mut crds = gossip.crds.write();
            for ix in 0..MAX_DUPLICATE_SHREDS {
                let mut chunk = seed.clone();
                // The last index is oldest, so the new proof wraps around to 0.
                chunk.wallclock = u64::from((ix + 1) % MAX_DUPLICATE_SHREDS);
                crds.insert(
                    CrdsValue::new_unsigned(CrdsData::DuplicateShred(ix, chunk)),
                    0,
                    GossipRoute::LocalMessage,
                )
                .expect("seed index is vacant");
            }
        }
        // Replace a record during validation with one newer than the signed chunks.
        let leader_schedule = |_| {
            if let Some(ix) = newer_record {
                let mut chunk = seed.clone();
                std::thread::sleep(Duration::from_millis(1));
                chunk.wallclock = timestamp() + clock_offset;
                gossip
                    .crds
                    .write()
                    .insert(
                        CrdsValue::new_unsigned(CrdsData::DuplicateShred(ix, chunk)),
                        0,
                        GossipRoute::LocalMessage,
                    )
                    .expect("newer record replaces seed");
                std::thread::sleep(Duration::from_millis(1));
            }
            Some(leader.pubkey())
        };
        gossip
            .push_duplicate_shred(
                &keypair,
                &shred,
                other.payload(),
                Some(leader_schedule),
                DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
                0,
            )
            .expect("publish despite possible clock skew");
        let offset = if newer_record == Some(MAX_DUPLICATE_SHREDS - 1) {
            0
        } else {
            MAX_DUPLICATE_SHREDS - 1
        };
        let crds = gossip.crds.read();
        assert_eq!(
            crds.len(),
            usize::from(MAX_DUPLICATE_SHREDS),
            "overwriting must preserve table size",
        );
        for ix in 0..MAX_DUPLICATE_SHREDS {
            let (entry, chunk) = get_chunk(&crds, keypair.pubkey(), ix);
            let chunk_index = (ix + MAX_DUPLICATE_SHREDS - offset) % MAX_DUPLICATE_SHREDS;
            if clock_offset > 0 && Some(ix) == newer_record {
                assert_eq!(chunk.slot, seed.slot, "future record must remain unchanged");
            } else if chunk_index < num_chunks {
                assert_eq!(chunk.slot, shred.slot(), "wrong slot at index {ix}");
                assert_eq!(
                    u16::from(chunk.chunk_index()),
                    chunk_index,
                    "wrong wrapped chunk index at {ix}",
                );
                assert!(entry.verify(), "invalid signature at index {ix}");
            } else {
                assert_eq!(chunk.slot, seed.slot, "unselected record {ix} changed slot");
            }
        }
    }

    #[test]
    fn test_get_gossip_nodes() {
        let mut rng = rand::rng();
        let now = timestamp();
        let self_pubkey = Pubkey::new_unique();
        let crds = RwLock::<Crds>::default();
        {
            let mut crds = crds.write();
            let mut nodes = vec![ContactInfo::new_rand(&mut rng, Some(self_pubkey))];
            nodes.extend(
                repeat_with(|| ContactInfo::new_rand(&mut rng, None)).take(4 * LOCK_CHUNK_SIZE),
            );
            for (k, mut node) in nodes.into_iter().enumerate() {
                node.set_shred_version((k as u16 + 1) % 3);
                let value = CrdsValue::new_unsigned(CrdsData::from(node));
                crds.insert(value, now, GossipRoute::LocalMessage).unwrap();
            }
            // Remove some nodes to reorder the rest.
            let removed: Vec<_> = crds
                .get_nodes()
                .skip(1)
                .step_by(7)
                .map(|v| v.value.label())
                .collect();
            for key in removed {
                crds.remove(&key, now);
            }
        }
        let num_nodes = crds.read().num_nodes();
        assert!(num_nodes > 3 * LOCK_CHUNK_SIZE, "num nodes: {num_nodes}");
        let stakes: HashMap<Pubkey, u64> = crds
            .read()
            .get_nodes_contact_info()
            .step_by(2)
            .map(|node| (*node.pubkey(), 1))
            .collect();
        let nodes = get_gossip_nodes(
            &mut rng,
            now,
            &self_pubkey,
            |shred_version| shred_version == 1,
            &crds,
            None, // gossip_validators
            &stakes,
            &SocketAddrSpace::Unspecified,
        );
        let mut expected: Vec<GossipStakePubkey> = crds
            .read()
            .get_nodes_contact_info()
            .filter(|node| node.pubkey() != &self_pubkey && node.shred_version() == 1)
            .map(|node| {
                let stake = stakes.get(node.pubkey()).copied().unwrap_or_default();
                (node.gossip().unwrap(), stake, *node.pubkey())
            })
            .collect();
        // Node k has shred version (k + 1) % 3 and nodes with k % 7 == 1 were
        // removed. Of k = 1..=512, 170 have shred version 1 and 24 of those
        // were removed.
        assert_eq!(expected.len(), 146);
        expected.reverse();
        assert_eq!(nodes, expected);
    }

    #[test]
    fn test_prune_errors() {
        let crds_gossip = CrdsGossip::default();
        let keypair = Keypair::new();
        let id = keypair.pubkey();
        let ci = ContactInfo::new_localhost(&Pubkey::from([1; 32]), 0);
        let prune_pubkey = Pubkey::from([2; 32]);
        crds_gossip
            .crds
            .write()
            .insert(
                CrdsValue::new_unsigned(CrdsData::from(&ci)),
                0,
                GossipRoute::LocalMessage,
            )
            .unwrap();
        let ping_cache = PingCache::new(
            GOSSIP_PING_CACHE_TTL,
            GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS,
            128, // capacity (small for tests)
        );
        let ping_cache = Mutex::new(ping_cache);
        crds_gossip.refresh_push_active_set(
            &keypair,
            0,               // shred version
            &HashMap::new(), // stakes
            None,            // gossip validators
            &ping_cache,
            &mut Vec::new(), // pings
            &SocketAddrSpace::Unspecified,
        );
        let now = timestamp();
        //incorrect dest
        let mut res = crds_gossip.process_prune_msg(
            &id,
            ci.pubkey(),
            &Pubkey::from(hash(&[1; 32]).to_bytes()),
            &[prune_pubkey],
            now,
            now,
            &HashMap::<Pubkey, u64>::default(), // stakes
        );
        assert_eq!(res.err(), Some(CrdsGossipError::BadPruneDestination));
        //correct dest
        res = crds_gossip.process_prune_msg(
            &id,             // self_pubkey
            ci.pubkey(),     // peer
            &id,             // destination
            &[prune_pubkey], // origins
            now,
            now,
            &HashMap::<Pubkey, u64>::default(), // stakes
        );
        res.unwrap();
        //test timeout
        let timeout = now + crds_gossip.push.prune_timeout * 2;
        res = crds_gossip.process_prune_msg(
            &id,             // self_pubkey
            ci.pubkey(),     // peer
            &id,             // destination
            &[prune_pubkey], // origins
            now,
            timeout,
            &HashMap::<Pubkey, u64>::default(), // stakes
        );
        assert_eq!(res.err(), Some(CrdsGossipError::PruneMessageTimeout));
    }
}
