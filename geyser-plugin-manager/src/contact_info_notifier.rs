//! Dispatch thread that delivers gossip contact info updates to opted-in
//! Geyser plugins.
//!
//! Decoupled from gossip via a bounded channel: the gossip subsystem does
//! a single non-blocking `try_send` per accepted CRDS contact info insert
//! (see [`solana_gossip::contact_info_notifier`]), and a thread owned by
//! this notifier drains the channel and dispatches to plugins.
//!
//! ## Threading model
//!
//! The dispatch thread is the *only* place plugin code runs for contact
//! info notifications. Gossip never invokes plugin code directly. This
//! ensures a misbehaving plugin cannot stall the gossip subsystem.
//!
//! ## Semantic dedup
//!
//! Contact info is rebroadcast by every validator on a multi-second
//! cadence even when nothing meaningful has changed. Without dedup,
//! plugins would receive a steady stream of redundant notifications.
//!
//! The dispatch thread keeps a private bounded `LruCache<Pubkey, u64>`
//! of semantic fingerprints (a hash of socket addresses, shred_version,
//! version components, and outset — *not* wallclock, since wallclock
//! advances on every routine republish) and skips any notification
//! whose fingerprint matches the last one delivered for that identity.
//! Outset is included because a change there indicates a node restart
//! or identity transfer, which is meaningful signal even if no other
//! field happens to differ.
//!
//! Because the cache is owned by a single thread, no lock is needed;
//! the LRU bound prevents long-running validators from accumulating
//! fingerprint state without bound as cluster membership churns.
//!
//! ## Startup replay (single-shot)
//!
//! When the dispatch thread starts, it first delivers the caller-supplied
//! initial state with `is_startup=true`, then transitions to streaming
//! live updates with `is_startup=false`. Mid-run plugin reload does *not*
//! trigger a fresh replay — see [`spawn`] docs.
//!
//! ## Drop semantics
//!
//! The thread exits cleanly when the channel disconnects (i.e. the
//! sender is dropped). Validator shutdown drops the sender held by
//! `Crds`, which causes the recv to fail and the thread to terminate.

use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaContactInfoV0_0_1, ReplicaContactInfoVersions,
    },
    arc_swap::ArcSwap,
    lazy_lru::LruCache,
    log::*,
    solana_gossip::contact_info_notifier::{
        ContactInfoEvent, ContactInfoReceiver, ContactInfoSender, ContactInfoSnapshot,
    },
    solana_pubkey::Pubkey,
    std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
        sync::Arc,
        thread::{self, JoinHandle},
    },
};

/// Default capacity for the gossip → dispatch channel. Sized to absorb
/// roughly two seconds of steady-state gossip traffic at mainnet scale
/// (~2,000 validators × 1 republish per ~6 s ≈ 333/s). Override by
/// passing a different value to [`attach`] or [`channel`].
pub const DEFAULT_CHANNEL_CAPACITY: usize = 4096;

/// Capacity of the per-identity semantic-fingerprint dedup cache held by
/// the dispatch thread. Sized at ~2× the protocol-level validator
/// admission cap so that recently-departed validators stay in the cache
/// long enough to avoid spurious "first-seen" notifications when they
/// rejoin, while still bounding long-term memory growth on
/// long-running validators.
const FINGERPRINT_CACHE_CAPACITY: usize = 4096;

/// Owns the dispatch thread for contact info notifications.
///
/// Drop this to terminate the thread (the sender will be dropped, the
/// channel disconnects, the thread exits).
pub struct ContactInfoNotifier {
    join_handle: Option<JoinHandle<()>>,
}

impl ContactInfoNotifier {
    /// Spawn the dispatch thread.
    ///
    /// `initial_state` is delivered to all opted-in plugins with
    /// `is_startup=true` *before* any live updates are processed. After
    /// the initial state is exhausted, the thread switches to draining
    /// the receiver and delivering live updates with `is_startup=false`.
    ///
    /// **Caveat:** this is a single-shot startup replay. Plugins loaded
    /// after the dispatch thread starts will only see live updates from
    /// the moment of their load forward; they do *not* receive a
    /// retroactive dump of the current CRDS state. This matches Option A
    /// from the design discussion: the common deployment pattern is to
    /// restart the validator when reconfiguring plugins, and gossip
    /// self-heals within one rebroadcast cycle (~6 s) for any
    /// validator that gossips again.
    pub fn spawn(
        plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
        initial_state: Vec<ContactInfoSnapshot>,
        receiver: ContactInfoReceiver,
    ) -> Self {
        let join_handle = thread::Builder::new()
            .name("solGeyserGossip".to_string())
            .spawn(move || run_dispatch_loop(plugin_manager, initial_state, receiver))
            .expect("failed to spawn contact info notifier thread");
        Self {
            join_handle: Some(join_handle),
        }
    }

    /// Wait for the dispatch thread to exit. Useful in tests; in
    /// production the validator shutdown path should drop the sender,
    /// which will cause the thread to exit on its own.
    pub fn join(mut self) -> thread::Result<()> {
        match self.join_handle.take() {
            Some(handle) => handle.join(),
            None => Ok(()),
        }
    }
}

/// Create a bounded channel suitable for use as the gossip → dispatch
/// transport. The sender should be installed into `Crds` via
/// `set_contact_info_sender`; the receiver is passed to
/// [`ContactInfoNotifier::spawn`].
pub fn channel(capacity: usize) -> (ContactInfoSender, ContactInfoReceiver) {
    crossbeam_channel::bounded(capacity)
}

/// Returns true if any loaded plugin opts into contact info notifications.
/// When this returns false, the dispatch thread should not be spawned and
/// no sender should be attached to gossip — gossip's hot path remains
/// completely unaffected.
pub fn any_plugin_opts_in(plugin_manager: &GeyserPluginManager) -> bool {
    plugin_manager
        .plugins
        .iter()
        .any(|p| p.contact_info_notifications_enabled())
}

/// Wire up contact info notifications end-to-end if (and only if) any
/// loaded plugin opts in. Returns `None` (and does nothing) otherwise,
/// leaving gossip and the validator process completely undisturbed.
///
/// On success, this function:
///
/// 1. Creates a bounded channel of the requested capacity.
/// 2. Attaches the sender to `cluster_info`'s underlying CRDS table so
///    every accepted contact info insert produces a snapshot event.
/// 3. Walks the current CRDS state (filtered to the local shred
///    version, matching the convention used by `getClusterNodes`) and
///    captures it as the initial replay set.
/// 4. Spawns the dispatch thread, which will deliver the initial set
///    with `is_startup=true` before transitioning to live updates.
///
/// The returned `ContactInfoNotifier` should be held for the lifetime
/// of the validator. Dropping it (or the underlying sender held by
/// gossip) terminates the dispatch thread.
pub fn attach(
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
    cluster_info: &solana_gossip::cluster_info::ClusterInfo,
    capacity: usize,
) -> Option<ContactInfoNotifier> {
    if !any_plugin_opts_in(&plugin_manager.load()) {
        return None;
    }
    let (sender, receiver) = channel(capacity);
    cluster_info.set_contact_info_sender(sender);
    let initial_state: Vec<ContactInfoSnapshot> = cluster_info
        .all_peers()
        .into_iter()
        .map(|(info, _)| ContactInfoSnapshot::from(&info))
        .collect();
    Some(ContactInfoNotifier::spawn(
        plugin_manager,
        initial_state,
        receiver,
    ))
}

fn run_dispatch_loop(
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
    initial_state: Vec<ContactInfoSnapshot>,
    receiver: ContactInfoReceiver,
) {
    // Per-identity semantic-fingerprint dedup. Bounded LRU rather than
    // an unbounded HashMap so a long-running validator that observes
    // churn over months doesn't accumulate state without bound.
    let mut fingerprints: LruCache<Pubkey, u64> = LruCache::new(FINGERPRINT_CACHE_CAPACITY);

    // Phase 1: deliver initial state with is_startup=true.
    for snapshot in initial_state {
        let fp = semantic_fingerprint(&snapshot);
        fingerprints.put(snapshot.pubkey, fp);
        dispatch_updated(&plugin_manager, &snapshot, /* is_startup */ true);
    }

    // Phase 2: drain the live channel until disconnect.
    while let Ok(event) = receiver.recv() {
        match event {
            ContactInfoEvent::Updated(snapshot) => {
                let fp = semantic_fingerprint(&snapshot);
                let unchanged = fingerprints.get(&snapshot.pubkey).copied() == Some(fp);
                if unchanged {
                    continue;
                }
                fingerprints.put(snapshot.pubkey, fp);
                dispatch_updated(&plugin_manager, &snapshot, /* is_startup */ false);
            }
            ContactInfoEvent::Removed(pubkey) => {
                // Drop the fingerprint so that if this identity later
                // rejoins the cluster, the next `Updated` event is
                // delivered (not suppressed by a stale fingerprint).
                fingerprints.pop(&pubkey);
                dispatch_removed(&plugin_manager, &pubkey);
            }
        }
    }
}

/// Build a `ReplicaContactInfoV0_0_1` view onto the snapshot and forward
/// to every plugin that opted in. Errors from individual plugins are
/// logged; other plugins continue to be called.
fn dispatch_updated(
    plugin_manager: &Arc<ArcSwap<GeyserPluginManager>>,
    snapshot: &ContactInfoSnapshot,
    is_startup: bool,
) {
    let plugin_manager = plugin_manager.load();
    if plugin_manager.plugins.is_empty() {
        return;
    }

    // The view is built entirely from `Copy` fields on the snapshot, so
    // dispatch is allocation-free — no `String` formatting, no Vec, no
    // owned types. This is the payoff for keeping the snapshot itself
    // free of heap data.
    let pubkey_bytes = snapshot.pubkey.as_ref();
    let view = ReplicaContactInfoV0_0_1 {
        pubkey: pubkey_bytes,
        wallclock: snapshot.wallclock,
        outset: snapshot.outset,
        shred_version: snapshot.shred_version,
        version_major: snapshot.version_major,
        version_minor: snapshot.version_minor,
        version_patch: snapshot.version_patch,
        version_commit: snapshot.version_commit,
        version_feature_set: snapshot.version_feature_set,
        version_client_id: snapshot.version_client_id,
        gossip: snapshot.gossip,
        tpu_quic: snapshot.tpu_quic,
        tpu_forwards_quic: snapshot.tpu_forwards_quic,
        tpu_vote_udp: snapshot.tpu_vote_udp,
        tpu_vote_quic: snapshot.tpu_vote_quic,
        tvu_udp: snapshot.tvu_udp,
        tvu_quic: snapshot.tvu_quic,
        serve_repair_udp: snapshot.serve_repair_udp,
        serve_repair_quic: snapshot.serve_repair_quic,
        rpc: snapshot.rpc,
        rpc_pubsub: snapshot.rpc_pubsub,
        alpenglow: snapshot.alpenglow,
    };

    for plugin in plugin_manager.plugins.iter() {
        if !plugin.contact_info_notifications_enabled() {
            continue;
        }
        let result =
            plugin.notify_contact_info(ReplicaContactInfoVersions::V0_0_1(&view), is_startup);
        if let Err(err) = result {
            error!(
                "Failed to notify contact info for {} to plugin {}: {}",
                snapshot.pubkey,
                plugin.name(),
                err,
            );
        }
    }
}

/// Forward a CRDS removal event to every opted-in plugin so consumers
/// can invalidate cached endpoints for the identity. Errors are logged;
/// other plugins continue to be called.
fn dispatch_removed(plugin_manager: &Arc<ArcSwap<GeyserPluginManager>>, pubkey: &Pubkey) {
    let plugin_manager = plugin_manager.load();
    if plugin_manager.plugins.is_empty() {
        return;
    }
    let pubkey_bytes = pubkey.as_ref();
    for plugin in plugin_manager.plugins.iter() {
        if !plugin.contact_info_notifications_enabled() {
            continue;
        }
        if let Err(err) = plugin.notify_contact_info_removed(pubkey_bytes) {
            error!(
                "Failed to notify contact info removal for {} to plugin {}: {}",
                pubkey,
                plugin.name(),
                err,
            );
        }
    }
}

/// Hash of the fields that meaningfully describe a validator's network
/// presence. Excludes `pubkey` (it's the cache key) and `wallclock`
/// (which advances on every republish without semantic change). Includes
/// `outset` because a change there indicates a node restart or identity
/// transfer — both of which are real events consumers want to see, even
/// if no other field happens to differ.
///
/// The exhaustive destructure pattern below forces a compile error if a
/// new field is added to `ContactInfoSnapshot`, so the fingerprint can
/// never silently miss a new socket or version field.
fn semantic_fingerprint(s: &ContactInfoSnapshot) -> u64 {
    let ContactInfoSnapshot {
        pubkey: _,
        wallclock: _,
        outset,
        shred_version,
        version_major,
        version_minor,
        version_patch,
        version_commit,
        version_feature_set,
        version_client_id,
        gossip,
        tpu_quic,
        tpu_forwards_quic,
        tpu_vote_udp,
        tpu_vote_quic,
        tvu_udp,
        tvu_quic,
        serve_repair_udp,
        serve_repair_quic,
        rpc,
        rpc_pubsub,
        alpenglow,
    } = s;
    let mut hasher = DefaultHasher::new();
    outset.hash(&mut hasher);
    shred_version.hash(&mut hasher);
    version_major.hash(&mut hasher);
    version_minor.hash(&mut hasher);
    version_patch.hash(&mut hasher);
    version_commit.hash(&mut hasher);
    version_feature_set.hash(&mut hasher);
    version_client_id.hash(&mut hasher);
    gossip.hash(&mut hasher);
    tpu_quic.hash(&mut hasher);
    tpu_forwards_quic.hash(&mut hasher);
    tpu_vote_udp.hash(&mut hasher);
    tpu_vote_quic.hash(&mut hasher);
    tvu_udp.hash(&mut hasher);
    tvu_quic.hash(&mut hasher);
    serve_repair_udp.hash(&mut hasher);
    serve_repair_quic.hash(&mut hasher);
    rpc.hash(&mut hasher);
    rpc_pubsub.hash(&mut hasher);
    alpenglow.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::geyser_plugin_manager::{GeyserPluginManager, LoadedGeyserPlugin},
        agave_geyser_plugin_interface::geyser_plugin_interface::{
            GeyserPlugin, ReplicaContactInfoVersions,
        },
        libloading::Library,
        std::{
            net::{IpAddr, Ipv4Addr, SocketAddr},
            sync::{
                Arc, Mutex,
                atomic::{AtomicUsize, Ordering},
            },
        },
    };

    #[derive(Debug)]
    struct RecordingPlugin {
        name: &'static str,
        enabled: bool,
        live_count: Arc<AtomicUsize>,
        startup_count: Arc<AtomicUsize>,
        removed_count: Arc<AtomicUsize>,
        last_pubkey: Arc<Mutex<Option<Vec<u8>>>>,
        last_removed_pubkey: Arc<Mutex<Option<Vec<u8>>>>,
    }

    impl GeyserPlugin for RecordingPlugin {
        fn name(&self) -> &'static str {
            self.name
        }

        fn contact_info_notifications_enabled(&self) -> bool {
            self.enabled
        }

        fn notify_contact_info(
            &self,
            info: ReplicaContactInfoVersions,
            is_startup: bool,
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
            let ReplicaContactInfoVersions::V0_0_1(info) = info;
            *self.last_pubkey.lock().unwrap() = Some(info.pubkey.to_vec());
            if is_startup {
                self.startup_count.fetch_add(1, Ordering::Relaxed);
            } else {
                self.live_count.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }

        fn notify_contact_info_removed(
            &self,
            pubkey: &[u8],
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
            *self.last_removed_pubkey.lock().unwrap() = Some(pubkey.to_vec());
            self.removed_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    /// Build a `RecordingPlugin` with zeroed counters and a shared
    /// "last seen" slot, so call sites only have to pass the bits that
    /// vary (name, enabled, optional shared counters).
    fn recording_plugin(
        name: &'static str,
        enabled: bool,
        live_count: Arc<AtomicUsize>,
        startup_count: Arc<AtomicUsize>,
        removed_count: Arc<AtomicUsize>,
    ) -> RecordingPlugin {
        RecordingPlugin {
            name,
            enabled,
            live_count,
            startup_count,
            removed_count,
            last_pubkey: Arc::new(Mutex::new(None)),
            last_removed_pubkey: Arc::new(Mutex::new(None)),
        }
    }

    fn loaded(plugin: RecordingPlugin) -> Arc<LoadedGeyserPlugin> {
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap();
        Arc::new(LoadedGeyserPlugin::new(
            Library::from(library),
            Box::new(plugin),
            None,
        ))
    }

    fn make_snapshot(pubkey: Pubkey, port: u16) -> ContactInfoSnapshot {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        ContactInfoSnapshot {
            pubkey,
            wallclock: 0,
            outset: 0,
            shred_version: 1,
            version_major: 0,
            version_minor: 0,
            version_patch: 0,
            version_commit: 0,
            version_feature_set: 0,
            version_client_id: 0,
            gossip: Some(addr),
            tpu_quic: Some(addr),
            tpu_forwards_quic: None,
            tpu_vote_udp: None,
            tpu_vote_quic: None,
            tvu_udp: None,
            tvu_quic: None,
            serve_repair_udp: None,
            serve_repair_quic: None,
            rpc: None,
            rpc_pubsub: None,
            alpenglow: None,
        }
    }

    #[test]
    fn delivers_startup_then_live() {
        let live = Arc::new(AtomicUsize::new(0));
        let startup = Arc::new(AtomicUsize::new(0));
        let removed = Arc::new(AtomicUsize::new(0));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![loaded(recording_plugin(
                "recorder",
                true,
                live.clone(),
                startup.clone(),
                removed.clone(),
            ))],
        })));

        let pk_a = Pubkey::new_unique();
        let pk_b = Pubkey::new_unique();
        let initial = vec![make_snapshot(pk_a, 8000), make_snapshot(pk_b, 8001)];

        let (sender, receiver) = channel(64);
        let notifier = ContactInfoNotifier::spawn(plugin_manager, initial, receiver);

        // Send a live update for pk_a (different port — semantic change)
        sender
            .send(ContactInfoEvent::Updated(make_snapshot(pk_a, 9000)))
            .unwrap();
        drop(sender);

        notifier.join().unwrap();

        assert_eq!(startup.load(Ordering::Relaxed), 2);
        assert_eq!(live.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn semantic_dedup_skips_identical_republish() {
        let live = Arc::new(AtomicUsize::new(0));
        let startup = Arc::new(AtomicUsize::new(0));
        let removed = Arc::new(AtomicUsize::new(0));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![loaded(recording_plugin(
                "recorder",
                true,
                live.clone(),
                startup.clone(),
                removed.clone(),
            ))],
        })));

        let pk = Pubkey::new_unique();
        let (sender, receiver) = channel(64);
        let notifier = ContactInfoNotifier::spawn(plugin_manager, vec![], receiver);

        // Same semantic content, just a wallclock advance. Dedup should suppress.
        let mut snap = make_snapshot(pk, 8000);
        sender.send(ContactInfoEvent::Updated(snap)).unwrap();
        snap.wallclock = 100;
        sender.send(ContactInfoEvent::Updated(snap)).unwrap();
        snap.wallclock = 200;
        sender.send(ContactInfoEvent::Updated(snap)).unwrap();
        drop(sender);

        notifier.join().unwrap();
        assert_eq!(startup.load(Ordering::Relaxed), 0);
        assert_eq!(live.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn skips_disabled_plugins() {
        let enabled_count = Arc::new(AtomicUsize::new(0));
        let disabled_count = Arc::new(AtomicUsize::new(0));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![
                loaded(recording_plugin(
                    "enabled",
                    true,
                    enabled_count.clone(),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                )),
                loaded(recording_plugin(
                    "disabled",
                    false,
                    disabled_count.clone(),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                )),
            ],
        })));

        let pk = Pubkey::new_unique();
        let (sender, receiver) = channel(64);
        let notifier = ContactInfoNotifier::spawn(plugin_manager, vec![], receiver);

        sender
            .send(ContactInfoEvent::Updated(make_snapshot(pk, 8000)))
            .unwrap();
        drop(sender);

        notifier.join().unwrap();
        assert_eq!(enabled_count.load(Ordering::Relaxed), 1);
        assert_eq!(disabled_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn any_plugin_opts_in_returns_correct_value() {
        let none_enabled = GeyserPluginManager {
            plugins: vec![loaded(recording_plugin(
                "off",
                false,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
            ))],
        };
        assert!(!any_plugin_opts_in(&none_enabled));

        let one_enabled = GeyserPluginManager {
            plugins: vec![
                loaded(recording_plugin(
                    "off",
                    false,
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                )),
                loaded(recording_plugin(
                    "on",
                    true,
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                )),
            ],
        };
        assert!(any_plugin_opts_in(&one_enabled));
    }

    #[test]
    fn removal_event_invokes_removed_callback_and_clears_fingerprint() {
        let live = Arc::new(AtomicUsize::new(0));
        let startup = Arc::new(AtomicUsize::new(0));
        let removed = Arc::new(AtomicUsize::new(0));
        let plugin = recording_plugin(
            "recorder",
            true,
            live.clone(),
            startup.clone(),
            removed.clone(),
        );
        let last_removed_pubkey = plugin.last_removed_pubkey.clone();
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![loaded(plugin)],
        })));

        let pk = Pubkey::new_unique();
        let (sender, receiver) = channel(64);
        let notifier = ContactInfoNotifier::spawn(plugin_manager, vec![], receiver);

        // Update → Update (semantic dup, suppressed) → Removed → Update
        // (must fire again because the fingerprint was cleared by the
        // Removed event).
        sender
            .send(ContactInfoEvent::Updated(make_snapshot(pk, 8000)))
            .unwrap();
        sender
            .send(ContactInfoEvent::Updated(make_snapshot(pk, 8000)))
            .unwrap();
        sender.send(ContactInfoEvent::Removed(pk)).unwrap();
        sender
            .send(ContactInfoEvent::Updated(make_snapshot(pk, 8000)))
            .unwrap();
        drop(sender);

        notifier.join().unwrap();

        // Two Updated events fired (one before remove, one after); the
        // republish in between was suppressed by semantic dedup.
        assert_eq!(live.load(Ordering::Relaxed), 2);
        assert_eq!(startup.load(Ordering::Relaxed), 0);
        // Exactly one Removed event reached the plugin, carrying the
        // correct identity pubkey.
        assert_eq!(removed.load(Ordering::Relaxed), 1);
        assert_eq!(
            last_removed_pubkey.lock().unwrap().as_deref(),
            Some(pk.as_ref()),
        );
    }
}
