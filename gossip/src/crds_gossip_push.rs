//! Crds Gossip Push overlay.
//!
//! This module is used to propagate recently created CrdsValues across the network
//! Eager push strategy is based on [Plumtree].
//!
//! [Plumtree]: http://asc.di.fct.unl.pt/~jleitao/pdf/srds07-leitao.pdf
//!
//! Main differences are:
//!
//! 1. There is no `max hop`.  Messages are signed with a local wallclock.  If they are outside of
//!    the local nodes wallclock window they are dropped silently.
//! 2. The prune set is stored in a Bloom filter.

use {
    crate::{
        cluster_info::CRDS_UNIQUE_PUBKEY_CAPACITY,
        cluster_info_metrics::{log_gossip_crds_sample_egress, should_report_message_signature},
        crds::{
            Crds, CrdsError, Cursor, GossipRoute, LOCK_CHUNK_SIZE, SIGNATURE_SAMPLE_LEADING_ZEROS,
        },
        crds_gossip,
        crds_value::CrdsValue,
        protocol::{Ping, PingCache},
        push_active_set::PushActiveSet,
        received_cache::ReceivedCache,
    },
    itertools::Itertools,
    parking_lot::RwLock,
    rand::SeedableRng,
    rand_chacha::ChaCha8Rng,
    solana_keypair::Keypair,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_time_utils::timestamp,
    std::{
        collections::{HashMap, HashSet},
        iter::repeat,
        net::SocketAddr,
        ops::RangeBounds,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    },
};

const CRDS_GOSSIP_PUSH_FANOUT: usize = 9;
// With a fanout of 9, a 2000 node cluster should only take ~3.5 hops to converge.
// However since pushes are stake weighed, some trailing nodes
// might need more time to receive values. 15 seconds should be plenty.
pub const CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS: u64 = 15000;
const CRDS_GOSSIP_PRUNE_MSG_TIMEOUT_MS: u64 = 500;
const CRDS_GOSSIP_PRUNE_STAKE_THRESHOLD_PCT: f64 = 0.15;
const CRDS_GOSSIP_PRUNE_MIN_INGRESS_NODES: usize = 2;
const CRDS_GOSSIP_PUSH_ACTIVE_SET_SIZE: usize = CRDS_GOSSIP_PUSH_FANOUT + 3;

pub struct CrdsGossipPush {
    /// Active set of validators for push
    active_set: RwLock<PushActiveSet>,
    /// Cursor into the crds table for values to push.
    crds_cursor: Mutex<Cursor>,
    /// Cache that tracks which validators a message was received from
    /// This cache represents a lagging view of which validators
    /// currently have this node in their `active_set`
    received_cache: Mutex<ReceivedCache>,
    push_fanout: usize,
    pub(crate) msg_timeout: u64,
    pub prune_timeout: u64,
    pub num_total: AtomicUsize,
    pub num_old: AtomicUsize,
    pub num_pushes: AtomicUsize,
}

impl Default for CrdsGossipPush {
    fn default() -> Self {
        Self {
            active_set: RwLock::default(),
            crds_cursor: Mutex::default(),
            received_cache: Mutex::new(ReceivedCache::new(CRDS_UNIQUE_PUBKEY_CAPACITY)),
            push_fanout: CRDS_GOSSIP_PUSH_FANOUT,
            msg_timeout: CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS,
            prune_timeout: CRDS_GOSSIP_PRUNE_MSG_TIMEOUT_MS,
            num_total: AtomicUsize::default(),
            num_old: AtomicUsize::default(),
            num_pushes: AtomicUsize::default(),
        }
    }
}
impl CrdsGossipPush {
    pub fn num_pending(&self, crds: &RwLock<Crds>) -> usize {
        let mut cursor: Cursor = *self.crds_cursor.lock().unwrap();
        crds.read().get_entries(&mut cursor).count()
    }

    pub(crate) fn prune_received_cache<I>(
        &self,
        self_pubkey: &Pubkey,
        origins: I, // Unique pubkeys of crds values' owners.
        stakes: &HashMap<Pubkey, u64>,
    ) -> HashMap</*gossip peer:*/ Pubkey, /*origins:*/ Vec<Pubkey>>
    where
        I: IntoIterator<Item = Pubkey>,
    {
        let mut received_cache = self.received_cache.lock().unwrap();
        origins
            .into_iter()
            .flat_map(|origin| {
                received_cache
                    .prune(
                        self_pubkey,
                        origin,
                        CRDS_GOSSIP_PRUNE_STAKE_THRESHOLD_PCT,
                        CRDS_GOSSIP_PRUNE_MIN_INGRESS_NODES,
                        stakes,
                    )
                    .zip(repeat(origin))
            })
            .into_group_map()
    }

    fn wallclock_window(&self, now: u64) -> impl RangeBounds<u64> + use<> {
        now.saturating_sub(self.msg_timeout)..=now.saturating_add(self.msg_timeout)
    }

    /// Process a push message to the network.
    ///
    /// Returns origins' pubkeys of upserted values.
    pub(crate) fn process_push_message(
        &self,
        crds: &RwLock<Crds>,
        messages: Vec<(/*from:*/ Pubkey, Vec<CrdsValue>)>,
        now: u64,
    ) -> HashSet<Pubkey> {
        let mut received_cache = self.received_cache.lock().unwrap();
        let mut crds = crds.write();
        let wallclock_window = self.wallclock_window(now);
        let mut origins = HashSet::new();
        for (from, values) in messages {
            self.num_total.fetch_add(values.len(), Ordering::Relaxed);
            for value in values {
                if !wallclock_window.contains(&value.wallclock()) {
                    continue;
                }
                let origin = value.pubkey();
                match crds.insert(value, now, GossipRoute::PushMessage(&from)) {
                    Ok(()) => {
                        received_cache.record(origin, from, /*num_dups:*/ 0);
                        origins.insert(origin);
                    }
                    Err(CrdsError::DuplicatePush(num_dups)) => {
                        received_cache.record(origin, from, usize::from(num_dups));
                        self.num_old.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(CrdsError::InsertFailed) => {
                        received_cache.record(origin, from, /*num_dups:*/ usize::MAX);
                        self.num_old.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        origins
    }

    /// New push message to broadcast to peers.
    ///
    /// Returns a list of Pubkeys for the selected peers and a list of values to send to all the
    /// peers.
    /// The list of push messages is created such that all the randomly selected peers have not
    /// pruned the source addresses.
    pub(crate) fn new_push_messages(
        &self,
        pubkey: &Pubkey, // This node.
        crds: &RwLock<Crds>,
        now: u64,
        stakes: &HashMap<Pubkey, u64>,
        // Cheap predicate returning false if the CRDS value should be discarded.
        // Runs under a bounded CRDS read lock before copying payloads.
        should_retain_crds_value: impl Fn(&CrdsValue) -> bool,
    ) -> (
        Vec<CrdsValue>, // unique CrdsValues pushed out to peers
        // map of pubkeys to indices in Vec<CrdsValue> pushed to that peer
        HashMap<Pubkey, Vec</*index:*/ usize>>,
        usize, // number of push messages
    ) {
        const MAX_NUM_PUSHES: usize = 1 << 12;
        let mut num_pushes = 0;
        let mut values = Vec::new();
        let mut push_messages = HashMap::<Pubkey, Vec</*index:*/ usize>>::new();
        let wallclock_window = self.wallclock_window(now);
        let mut batch = Vec::with_capacity(LOCK_CHUNK_SIZE);
        let active_set = self.active_set.read();
        let has_active_peers = !active_set.is_empty();
        let mut crds_cursor = self.crds_cursor.lock().unwrap();
        // crds should be locked last after self.{active_set,crds_cursor}.
        // Defer concurrent inserts to the next call, bounding this scan even
        // when every value is discarded or all of its peers have pruned it.
        let end = crds.read().cursor();
        'outer: loop {
            let mut num_scanned = 0;
            let mut next_cursor = *crds_cursor;
            {
                let crds = crds.read();
                for (cursor, value) in crds
                    .get_entries_until(*crds_cursor, end)
                    .take(LOCK_CHUNK_SIZE)
                {
                    num_scanned += 1;
                    next_cursor = cursor;
                    // Reject before copying: discarded values and an empty
                    // active set should not allocate or clone payloads.
                    if wallclock_window.contains(&value.wallclock())
                        && should_retain_crds_value(value)
                        && has_active_peers
                    {
                        batch.push((cursor, value.clone()));
                    }
                }
            }
            for (cursor, value) in batch.drain(..) {
                // Commit only consumed entries: hitting MAX_NUM_PUSHES must
                // leave the rest of this batch pending for the next call.
                *crds_cursor = cursor;
                let origin = value.pubkey();
                let mut nodes = active_set
                    .get_nodes(pubkey, &origin, stakes)
                    .take(self.push_fanout)
                    .peekable();
                if nodes.peek().is_none() {
                    continue;
                }
                let index = values.len();
                values.push(value);
                let value = &values[index];
                let should_report = should_report_message_signature(
                    value.signature(),
                    SIGNATURE_SAMPLE_LEADING_ZEROS,
                );
                for &node in nodes {
                    if should_report {
                        log_gossip_crds_sample_egress(value, &node);
                    }
                    push_messages.entry(node).or_default().push(index);
                    num_pushes += 1;
                    if num_pushes >= MAX_NUM_PUSHES {
                        break 'outer;
                    }
                }
            }
            *crds_cursor = next_cursor;
            if num_scanned < LOCK_CHUNK_SIZE {
                break;
            }
        }
        drop(crds_cursor);
        drop(active_set);
        self.num_pushes.fetch_add(num_pushes, Ordering::Relaxed);
        (values, push_messages, num_pushes)
    }

    /// Add the `from` to the peer's filter of nodes.
    pub(crate) fn process_prune_msg(
        &self,
        self_pubkey: &Pubkey,
        peer: &Pubkey,
        origins: &[Pubkey],
        stakes: &HashMap<Pubkey, u64>,
    ) {
        let active_set = self.active_set.read();
        active_set.prune(self_pubkey, peer, origins, stakes);
    }

    /// Refresh the push active set.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn refresh_push_active_set(
        &self,
        crds: &RwLock<Crds>,
        stakes: &HashMap<Pubkey, u64>,
        gossip_validators: Option<&HashSet<Pubkey>>,
        self_keypair: &Keypair,
        self_shred_version: u16,
        ping_cache: &Mutex<PingCache>,
        pings: &mut Vec<(SocketAddr, Ping)>,
        socket_addr_space: &SocketAddrSpace,
    ) {
        let mut rng = rand::rng();
        // Active and valid gossip nodes with matching shred-version.
        let nodes = crds_gossip::get_gossip_nodes(
            &mut rng,
            timestamp(), // now
            &self_keypair.pubkey(),
            // Only push to nodes with the same shred version.
            |shred_version| shred_version == self_shred_version,
            crds,
            gossip_validators,
            stakes,
            socket_addr_space,
        );
        // Check for nodes which have responded to ping messages.
        let nodes = crds_gossip::maybe_ping_gossip_addresses(
            &mut rng,
            nodes,
            self_keypair,
            ping_cache,
            pings,
        );
        let nodes = crds_gossip::dedup_gossip_addresses(nodes)
            .into_iter()
            .map(|(_gossip, stake, pubkey)| (stake, pubkey));
        if nodes.len() == 0 {
            return;
        }
        let cluster_size = crds.read().num_pubkeys().max(stakes.len());
        let mut active_set = self.active_set.write();
        active_set.rotate(
            &mut ChaCha8Rng::from_rng(&mut rng),
            CRDS_GOSSIP_PUSH_ACTIVE_SET_SIZE,
            cluster_size,
            nodes,
        )
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            cluster_info::{GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS, GOSSIP_PING_CACHE_TTL},
            contact_info::ContactInfo,
            crds_data::CrdsData,
        },
        std::{cell::Cell, time::Instant},
    };

    fn new_ping_cache() -> PingCache {
        PingCache::new(
            GOSSIP_PING_CACHE_TTL,
            GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS,
            128, // capacity (small for tests)
        )
    }

    fn new_push_with_peers(num_peers: usize) -> (CrdsGossipPush, Pubkey) {
        let push = CrdsGossipPush::default();
        let pubkey = Pubkey::new_unique();
        push.active_set.write().rotate(
            &mut rand::rng(),
            num_peers,
            num_peers,
            (0..num_peers).map(|_| (0, Pubkey::new_unique())),
        );
        (push, pubkey)
    }

    #[test]
    fn test_new_push_messages_limit_keeps_unconsumed_batch_pending() {
        let (push, pubkey) = new_push_with_peers(CRDS_GOSSIP_PUSH_FANOUT);
        let stakes = HashMap::new();
        let now = timestamp();
        let mut crds = Crds::default();
        let mut expected = Vec::new();
        while expected.len() < 600 {
            let origin = Pubkey::new_unique();
            // Ensure each origin has exactly nine peers, avoiding Bloom false
            // positives in the fanout-limit assertions below.
            if push
                .active_set
                .read()
                .get_nodes(&pubkey, &origin, &stakes)
                .count()
                != CRDS_GOSSIP_PUSH_FANOUT
            {
                continue;
            }
            let value =
                CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(&origin, now)));
            crds.insert(value.clone(), now, GossipRoute::LocalMessage)
                .unwrap();
            expected.push(value);
        }
        let crds = RwLock::new(crds);
        let (mut values, messages, num_pushes) =
            push.new_push_messages(&pubkey, &crds, now, &stakes, |_| true);
        assert_eq!(num_pushes, 1 << 12);
        assert_eq!(messages.values().map(Vec::len).sum::<usize>(), num_pushes);
        assert_eq!(values.len(), 456);
        assert_eq!(push.num_pending(&crds), expected.len() - values.len());
        let (remaining, messages, num_pushes) =
            push.new_push_messages(&pubkey, &crds, now, &stakes, |_| true);
        assert_eq!(num_pushes, remaining.len() * CRDS_GOSSIP_PUSH_FANOUT);
        assert_eq!(messages.values().map(Vec::len).sum::<usize>(), num_pushes);
        values.extend(remaining);
        assert_eq!(values, expected);
        assert_eq!(push.num_pending(&crds), 0);
    }

    #[test]
    fn test_new_push_messages_consumes_rejected_batches() {
        let (push, pubkey) = new_push_with_peers(1);
        let now = timestamp();
        let mut crds = Crds::default();
        for ix in 0..3 * LOCK_CHUNK_SIZE {
            let wallclock = if ix < LOCK_CHUNK_SIZE { 0 } else { now };
            let value = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(
                &Pubkey::new_unique(),
                wallclock,
            )));
            crds.insert(value, now, GossipRoute::LocalMessage).unwrap();
        }
        let crds = RwLock::new(crds);
        let calls = Cell::new(0);
        let (values, messages, num_pushes) =
            push.new_push_messages(&pubkey, &crds, now, &HashMap::new(), |_| {
                calls.set(calls.get() + 1);
                false
            });
        assert_eq!(calls.get(), 2 * LOCK_CHUNK_SIZE);
        assert!(values.is_empty());
        assert!(messages.is_empty());
        assert_eq!(num_pushes, 0);
        assert_eq!(push.num_pending(&crds), 0);
    }

    impl CrdsGossipPush {
        // Wrapper for CrdsGossipPush.new_push_messages replicating old return
        // type for legacy tests.
        fn old_new_push_messages(
            &self,
            pubkey: &Pubkey,
            crds: &RwLock<Crds>,
            now: u64,
            stakes: &HashMap<Pubkey, u64>,
        ) -> HashMap<Pubkey, Vec<CrdsValue>> {
            let (entries, messages, _) = self.new_push_messages(
                pubkey,
                crds,
                now,
                stakes,
                |_| true, // should_retain_crds_value
            );
            messages
                .into_iter()
                .map(|(pubkey, indices)| {
                    let values = indices.into_iter().map(|k| entries[k].clone()).collect();
                    (pubkey, values)
                })
                .collect()
        }
    }

    #[test]
    fn test_process_push_one() {
        let crds = RwLock::<Crds>::default();
        let push = CrdsGossipPush::default();
        let value = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(
            &solana_pubkey::new_rand(),
            0,
        )));
        let label = value.label();
        // push a new message
        assert_eq!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value.clone()])], 0),
            [label.pubkey()].into_iter().collect(),
        );
        assert_eq!(crds.read().get::<&CrdsValue>(&label), Some(&value));

        // push it again
        assert!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value])], 0)
                .is_empty()
        );
    }
    #[test]
    fn test_process_push_old_version() {
        let crds = RwLock::<Crds>::default();
        let push = CrdsGossipPush::default();
        let mut ci = ContactInfo::new_localhost(&solana_pubkey::new_rand(), 0);
        ci.set_wallclock(1);
        let value = CrdsValue::new_unsigned(CrdsData::from(&ci));

        // push a new message
        assert_eq!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value])], 0),
            [*ci.pubkey()].into_iter().collect()
        );

        // push an old version
        ci.set_wallclock(0);
        let value = CrdsValue::new_unsigned(CrdsData::from(ci));
        assert!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value])], 0)
                .is_empty()
        );
    }
    #[test]
    fn test_process_push_timeout() {
        let crds = RwLock::<Crds>::default();
        let push = CrdsGossipPush::default();
        let timeout = push.msg_timeout;
        let mut ci = ContactInfo::new_localhost(&solana_pubkey::new_rand(), 0);

        // push a version to far in the future
        ci.set_wallclock(timeout + 1);
        let value = CrdsValue::new_unsigned(CrdsData::from(&ci));
        assert!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value])], 0)
                .is_empty()
        );

        // push a version to far in the past
        ci.set_wallclock(0);
        let value = CrdsValue::new_unsigned(CrdsData::from(ci));
        assert!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value])], timeout + 1)
                .is_empty()
        );
    }
    #[test]
    fn test_process_push_update() {
        let crds = RwLock::<Crds>::default();
        let push = CrdsGossipPush::default();
        let mut ci = ContactInfo::new_localhost(&solana_pubkey::new_rand(), 0);
        let origin = *ci.pubkey();
        ci.set_wallclock(0);
        let value_old = CrdsValue::new_unsigned(CrdsData::from(&ci));

        // push a new message
        assert_eq!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value_old])], 0),
            [origin].into_iter().collect()
        );

        // push an old version
        ci.set_wallclock(1);
        let value = CrdsValue::new_unsigned(CrdsData::from(ci));
        assert_eq!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value])], 0),
            [origin].into_iter().collect()
        );
    }

    #[test]
    fn test_new_push_messages() {
        let now = timestamp();
        let mut crds = Crds::default();
        let push = CrdsGossipPush::default();
        let mut ping_cache = new_ping_cache();
        let peer = ContactInfo::new_localhost(&solana_pubkey::new_rand(), 0);
        ping_cache.mock_pong(*peer.pubkey(), peer.gossip().unwrap(), Instant::now());
        let peer = CrdsValue::new_unsigned(CrdsData::from(peer));
        assert_eq!(
            crds.insert(peer.clone(), now, GossipRoute::LocalMessage),
            Ok(())
        );
        let crds = RwLock::new(crds);
        let ping_cache = Mutex::new(ping_cache);
        push.refresh_push_active_set(
            &crds,
            &HashMap::new(), // stakes
            None,            // gossip_validtors
            &Keypair::new(),
            0, // self_shred_version
            &ping_cache,
            &mut Vec::new(), // pings
            &SocketAddrSpace::Unspecified,
        );

        let new_msg = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(
            &solana_pubkey::new_rand(),
            0,
        )));
        let mut expected = HashMap::new();
        expected.insert(peer.label().pubkey(), vec![new_msg.clone()]);
        let origin = new_msg.pubkey();
        assert_eq!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![new_msg])], 0),
            [origin].into_iter().collect()
        );
        assert_eq!(
            push.old_new_push_messages(
                &Pubkey::default(),
                &crds,
                0,
                &HashMap::<Pubkey, u64>::default(), // stakes
            ),
            expected
        );
    }
    #[test]
    fn test_personalized_push_messages() {
        let now = timestamp();
        let mut rng = rand::rng();
        let mut crds = Crds::default();
        let push = CrdsGossipPush::default();
        let mut ping_cache = new_ping_cache();
        let peers: Vec<_> = vec![0, 0, now]
            .into_iter()
            .map(|wallclock| {
                let mut peer = ContactInfo::new_rand(&mut rng, /*pubkey=*/ None);
                peer.set_wallclock(wallclock);
                ping_cache.mock_pong(*peer.pubkey(), peer.gossip().unwrap(), Instant::now());
                CrdsValue::new_unsigned(CrdsData::from(peer))
            })
            .collect();
        let origin: Vec<_> = peers.iter().map(|node| node.pubkey()).collect();
        assert_eq!(
            crds.insert(peers[0].clone(), now, GossipRoute::LocalMessage),
            Ok(())
        );
        assert_eq!(
            crds.insert(peers[1].clone(), now, GossipRoute::LocalMessage),
            Ok(())
        );
        let crds = RwLock::new(crds);
        assert_eq!(
            push.process_push_message(
                &crds,
                vec![(Pubkey::default(), vec![peers[2].clone()])],
                now
            ),
            [origin[2]].into_iter().collect()
        );
        let ping_cache = Mutex::new(ping_cache);
        push.refresh_push_active_set(
            &crds,
            &HashMap::new(), // stakes
            None,            // gossip_validators
            &Keypair::new(),
            0, // self_shred_version
            &ping_cache,
            &mut Vec::new(),
            &SocketAddrSpace::Unspecified,
        );

        // push 3's contact info to 1 and 2 and 3
        let expected: HashMap<_, _> = vec![
            (peers[0].pubkey(), vec![peers[2].clone()]),
            (peers[1].pubkey(), vec![peers[2].clone()]),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            push.old_new_push_messages(
                &Pubkey::default(),
                &crds,
                now,
                &HashMap::<Pubkey, u64>::default(), // stakes
            ),
            expected
        );
    }
    #[test]
    fn test_process_prune() {
        let mut crds = Crds::default();
        let self_id = solana_pubkey::new_rand();
        let push = CrdsGossipPush::default();
        let peer = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(
            &solana_pubkey::new_rand(),
            0,
        )));
        assert_eq!(
            crds.insert(peer.clone(), 0, GossipRoute::LocalMessage),
            Ok(())
        );
        let crds = RwLock::new(crds);
        let ping_cache = Mutex::new(new_ping_cache());
        push.refresh_push_active_set(
            &crds,
            &HashMap::new(), // stakes
            None,            // gossip_validators
            &Keypair::new(),
            0, // self_shred_version
            &ping_cache,
            &mut Vec::new(), // pings
            &SocketAddrSpace::Unspecified,
        );

        let new_msg = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(
            &solana_pubkey::new_rand(),
            0,
        )));
        let expected = HashMap::new();
        let origin = new_msg.pubkey();
        assert_eq!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![new_msg.clone()])], 0),
            [origin].into_iter().collect()
        );
        push.process_prune_msg(
            &self_id,
            &peer.label().pubkey(),
            &[new_msg.label().pubkey()],
            &HashMap::<Pubkey, u64>::default(), // stakes
        );
        assert_eq!(
            push.old_new_push_messages(
                &self_id,
                &crds,
                0,
                &HashMap::<Pubkey, u64>::default(), // stakes
            ),
            expected
        );
    }
    #[test]
    fn test_purge_old_pending_push_messages() {
        let mut crds = Crds::default();
        let push = CrdsGossipPush::default();
        let peer = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(
            &solana_pubkey::new_rand(),
            0,
        )));
        assert_eq!(crds.insert(peer, 0, GossipRoute::LocalMessage), Ok(()));
        let crds = RwLock::new(crds);
        let ping_cache = Mutex::new(new_ping_cache());
        push.refresh_push_active_set(
            &crds,
            &HashMap::new(), // stakes
            None,            // gossip_validators
            &Keypair::new(),
            0, // self_shred_version
            &ping_cache,
            &mut Vec::new(), // pings
            &SocketAddrSpace::Unspecified,
        );

        let mut ci = ContactInfo::new_localhost(&solana_pubkey::new_rand(), 0);
        ci.set_wallclock(1);
        let new_msg = CrdsValue::new_unsigned(CrdsData::from(ci));
        let expected = HashMap::new();
        let origin = new_msg.pubkey();
        assert_eq!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![new_msg])], 1),
            [origin].into_iter().collect()
        );
        assert_eq!(
            push.old_new_push_messages(
                &Pubkey::default(),
                &crds,
                0,
                &HashMap::<Pubkey, u64>::default(), // stakes
            ),
            expected
        );
    }

    #[test]
    fn test_purge_old_received_cache() {
        let crds = RwLock::<Crds>::default();
        let push = CrdsGossipPush::default();
        let mut ci = ContactInfo::new_localhost(&solana_pubkey::new_rand(), 0);
        ci.set_wallclock(0);
        let value = CrdsValue::new_unsigned(CrdsData::from(ci));
        let label = value.label();
        // push a new message
        assert_eq!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value.clone()])], 0),
            [label.pubkey()].into_iter().collect()
        );
        assert_eq!(crds.write().get::<&CrdsValue>(&label), Some(&value));

        // push it again
        assert!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value.clone()])], 0)
                .is_empty()
        );

        // push it again
        assert!(
            push.process_push_message(&crds, vec![(Pubkey::default(), vec![value])], 0)
                .is_empty()
        );
    }
}
