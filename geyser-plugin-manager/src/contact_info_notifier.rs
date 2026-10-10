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
//! ## Republishes
//!
//! Subscribed plugins receive contact info republishes even when only the
//! wallclock changes. Plugins and downstream clients can learn about unchanged
//! nodes from later republishes. Consumers that only want changes to endpoints
//! or other fields can deduplicate the notifications themselves.
//!
//! Plugins should be prepared for thousands of notifications per second, many
//! carrying unchanged contact info. Callbacks run serially on the dispatch thread.
//! A blocking callback delays all subscribed plugins and can fill the shared
//! channel, causing gossip to drop subsequent updates. Keep callbacks short and
//! move expensive work to a plugin-owned queue. Delivery is best-effort; later
//! republishes may restore the current state, but do not recover every missed update.
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
    agave_geyser_plugin_host::GeyserPluginHost,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaContactInfoV0_0_1, ReplicaContactInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_gossip::contact_info_notifier::{
        ContactInfoEvent, ContactInfoReceiver, ContactInfoSender, ContactInfoSnapshot,
    },
    solana_pubkey::Pubkey,
    std::{
        sync::Arc,
        thread::{self, JoinHandle},
    },
};

/// Default capacity for the gossip-to-dispatch channel. Events are dropped
/// when the channel is full. Override by passing a different value to
/// [`attach`] or [`channel`].
pub const DEFAULT_CHANNEL_CAPACITY: usize = 4096;

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
    /// Plugins loaded after the dispatch thread starts receive live updates,
    /// including later republishes of unchanged contact info. They do not receive
    /// a fresh startup replay. Delivery is best-effort, so learning the current
    /// cluster state takes time and depends on subsequent gossip activity.
    pub fn spawn(
        plugin_manager: Arc<ArcSwap<GeyserPluginHost>>,
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
pub fn any_plugin_opts_in(plugin_manager: &GeyserPluginHost) -> bool {
    plugin_manager
        .plugins()
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
    plugin_manager: Arc<ArcSwap<GeyserPluginHost>>,
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
    plugin_manager: Arc<ArcSwap<GeyserPluginHost>>,
    initial_state: Vec<ContactInfoSnapshot>,
    receiver: ContactInfoReceiver,
) {
    for snapshot in initial_state {
        dispatch_updated(&plugin_manager, &snapshot, /* is_startup */ true);
    }

    while let Ok(event) = receiver.recv() {
        match event {
            ContactInfoEvent::Updated(snapshot) => {
                dispatch_updated(&plugin_manager, &snapshot, /* is_startup */ false);
            }
            ContactInfoEvent::Removed(pubkey) => {
                dispatch_removed(&plugin_manager, &pubkey);
            }
        }
    }
}

/// Build a `ReplicaContactInfoV0_0_1` view onto the snapshot and forward
/// to every plugin that opted in. Errors from individual plugins are
/// logged; other plugins continue to be called.
fn dispatch_updated(
    plugin_manager: &Arc<ArcSwap<GeyserPluginHost>>,
    snapshot: &ContactInfoSnapshot,
    is_startup: bool,
) {
    let plugin_manager = plugin_manager.load();
    if plugin_manager.plugins().is_empty() {
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

    for plugin in plugin_manager.plugins().iter() {
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
fn dispatch_removed(plugin_manager: &Arc<ArcSwap<GeyserPluginHost>>, pubkey: &Pubkey) {
    let plugin_manager = plugin_manager.load();
    if plugin_manager.plugins().is_empty() {
        return;
    }
    let pubkey_bytes = pubkey.as_ref();
    for plugin in plugin_manager.plugins().iter() {
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_geyser_plugin_host::{GeyserPluginHost, LoadedGeyserPlugin},
        agave_geyser_plugin_interface::geyser_plugin_interface::{
            GeyserPlugin, ReplicaContactInfoVersions,
        },
        crossbeam_channel::Sender,
        libloading::Library,
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_signer::Signer,
        std::{
            net::{IpAddr, Ipv4Addr, SocketAddr},
            sync::{
                Arc, Mutex,
                atomic::{AtomicUsize, Ordering},
            },
            time::Duration,
        },
    };

    #[derive(Debug)]
    struct RecordingPlugin {
        name: &'static str,
        enabled: bool,
        notification_sender: Option<Sender<u64>>,
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
            if let Some(sender) = &self.notification_sender {
                sender.send(info.wallclock).unwrap();
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
            notification_sender: None,
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
        // SAFETY: This plugin is defined in the current executable and starts no background work.
        Arc::new(unsafe { LoadedGeyserPlugin::new(Library::from(library), Box::new(plugin), None) })
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

    fn make_cluster_info() -> ClusterInfo {
        let keypair = Arc::new(Keypair::new());
        ClusterInfo::new(
            ContactInfo::new(keypair.pubkey(), 0, 1),
            keypair,
            SocketAddrSpace::Unspecified,
        )
    }

    #[test]
    fn attach_preserves_sender_when_no_plugin_opts_in() {
        let disabled = loaded(recording_plugin(
            "disabled",
            false,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ));
        for plugins in [vec![], vec![disabled]] {
            let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::from_plugins(
                plugins,
            ))));
            let cluster_info = make_cluster_info();
            let (sender, receiver) = channel(1);
            cluster_info.set_contact_info_sender(sender);

            assert!(attach(plugin_manager, &cluster_info, 4).is_none());

            let peer = Pubkey::new_unique();
            cluster_info.insert_info(ContactInfo::new(peer, 0, 1));
            let event = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(matches!(event, ContactInfoEvent::Updated(info) if info.pubkey == peer));
        }
    }

    #[test]
    fn attach_delivers_existing_and_new_peers() {
        let live = Arc::new(AtomicUsize::new(0));
        let startup = Arc::new(AtomicUsize::new(0));
        let plugin = recording_plugin(
            "recorder",
            true,
            live.clone(),
            startup.clone(),
            Arc::new(AtomicUsize::new(0)),
        );
        let last_pubkey = plugin.last_pubkey.clone();
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::from_plugins(
            vec![loaded(plugin)],
        ))));
        let cluster_info = make_cluster_info();
        cluster_info.insert_info(ContactInfo::new(Pubkey::new_unique(), 0, 1));

        let notifier = attach(plugin_manager, &cluster_info, 4).unwrap();
        let peer = Pubkey::new_unique();
        cluster_info.insert_info(ContactInfo::new(peer, 0, 1));
        drop(cluster_info);
        notifier.join().unwrap();

        assert_eq!(startup.load(Ordering::Relaxed), 2);
        assert_eq!(live.load(Ordering::Relaxed), 1);
        assert_eq!(last_pubkey.lock().unwrap().as_deref(), Some(peer.as_ref()));
    }

    #[test]
    fn delivers_startup_then_live() {
        let live = Arc::new(AtomicUsize::new(0));
        let startup = Arc::new(AtomicUsize::new(0));
        let removed = Arc::new(AtomicUsize::new(0));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::from_plugins(
            vec![loaded(recording_plugin(
                "recorder",
                true,
                live.clone(),
                startup.clone(),
                removed.clone(),
            ))],
        ))));

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
    fn delivers_republishes_to_all_subscribed_plugins() {
        let live_counts = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
        let startup_counts = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
        let plugins = live_counts
            .iter()
            .zip(&startup_counts)
            .map(|(live, startup)| {
                loaded(recording_plugin(
                    "recorder",
                    true,
                    live.clone(),
                    startup.clone(),
                    Arc::new(AtomicUsize::new(0)),
                ))
            })
            .collect();
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::from_plugins(
            plugins,
        ))));
        let mut snapshot = make_snapshot(Pubkey::new_unique(), 8000);
        let (sender, receiver) = channel(64);
        let notifier = ContactInfoNotifier::spawn(plugin_manager, vec![snapshot], receiver);

        for wallclock in [100, 200] {
            snapshot.wallclock = wallclock;
            sender.send(ContactInfoEvent::Updated(snapshot)).unwrap();
        }
        drop(sender);
        notifier.join().unwrap();

        for live in live_counts {
            assert_eq!(live.load(Ordering::Relaxed), 2);
        }
        for startup in startup_counts {
            assert_eq!(startup.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn plugin_loaded_after_startup_receives_republish() {
        let (notification_sender, notifications) = crossbeam_channel::bounded(1);
        let mut original = recording_plugin(
            "original",
            true,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        original.notification_sender = Some(notification_sender);
        let original = loaded(original);
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::from_plugins(
            vec![original.clone()],
        ))));
        let mut snapshot = make_snapshot(Pubkey::new_unique(), 8000);
        let (sender, receiver) = channel(64);
        let notifier = ContactInfoNotifier::spawn(plugin_manager.clone(), vec![snapshot], receiver);
        assert_eq!(
            notifications.recv_timeout(Duration::from_secs(5)).unwrap(),
            0
        );

        let live = Arc::new(AtomicUsize::new(0));
        let startup = Arc::new(AtomicUsize::new(0));
        let added = recording_plugin(
            "added",
            true,
            live.clone(),
            startup.clone(),
            Arc::new(AtomicUsize::new(0)),
        );
        let last_pubkey = added.last_pubkey.clone();
        plugin_manager.store(Arc::new(GeyserPluginHost::from_plugins(vec![
            original,
            loaded(added),
        ])));
        snapshot.wallclock = 100;
        sender.send(ContactInfoEvent::Updated(snapshot)).unwrap();
        drop(sender);
        notifier.join().unwrap();

        assert_eq!(live.load(Ordering::Relaxed), 1);
        assert_eq!(startup.load(Ordering::Relaxed), 0);
        assert_eq!(
            last_pubkey.lock().unwrap().as_deref(),
            Some(snapshot.pubkey.as_ref())
        );
        assert_eq!(notifications.recv().unwrap(), 100);
    }

    #[test]
    fn skips_disabled_plugins() {
        let enabled_count = Arc::new(AtomicUsize::new(0));
        let disabled_count = Arc::new(AtomicUsize::new(0));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::from_plugins(
            vec![
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
        ))));

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
        let none_enabled = GeyserPluginHost::from_plugins(vec![loaded(recording_plugin(
            "off",
            false,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ))]);
        assert!(!any_plugin_opts_in(&none_enabled));

        let one_enabled = GeyserPluginHost::from_plugins(vec![
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
        ]);
        assert!(any_plugin_opts_in(&one_enabled));
    }

    #[test]
    fn delivers_removal_and_subsequent_republishes() {
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
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::from_plugins(
            vec![loaded(plugin)],
        ))));

        let pk = Pubkey::new_unique();
        let (sender, receiver) = channel(64);
        let notifier = ContactInfoNotifier::spawn(plugin_manager, vec![], receiver);

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

        assert_eq!(live.load(Ordering::Relaxed), 3);
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
