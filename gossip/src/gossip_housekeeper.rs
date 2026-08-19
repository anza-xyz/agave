use {
    crate::{cluster_info::ClusterInfo, gossip_context::GossipContext, gossip_timer::Periodic},
    solana_streamer::streamer::StreamerReceiveStats,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle, sleep},
        time::{Duration, Instant},
    },
};

const METRICS_INTERVAL: Duration = Duration::from_secs(2);
/// How often the loop wakes to check its deadlines and the exit flag. All of
/// its work is coarse periodic reporting, so this only bounds shutdown latency.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Periodic read-only reporting: metrics, the contact-info trace, and the
/// contact-info save file.
///
/// None of this writes to the CRDS table, so it is kept off the engine thread
/// where it would stall gossip ingress. Stakes come from the shared
/// [`GossipContext`] snapshot so this thread reports the same epoch the engine
/// and ingress threads are using.
pub(crate) struct GossipHousekeeper {
    pub(crate) cluster_info: Arc<ClusterInfo>,
    pub(crate) context: Arc<GossipContext>,
    pub(crate) receiver_stats: Arc<StreamerReceiveStats>,
    pub(crate) exit: Arc<AtomicBool>,
}

impl GossipHousekeeper {
    pub(crate) fn spawn(self) -> JoinHandle<()> {
        Builder::new()
            .name("solGossipHouse".to_string())
            .spawn(move || {
                let Self {
                    cluster_info,
                    context,
                    receiver_stats,
                    exit,
                } = self;
                let start = Instant::now();
                let mut metrics = Periodic::due_after(start, METRICS_INTERVAL);
                let mut contact_trace = cluster_info
                    .contact_debug_interval()
                    .map(|period| Periodic::due_after(start, period));
                let mut contact_save = cluster_info
                    .contact_save_interval()
                    .map(|period| Periodic::due_after(start, period));

                while !exit.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    if metrics.claim(now) {
                        cluster_info.submit_stats(&context.load().stakes);
                        receiver_stats.report();
                    }
                    if contact_trace.as_mut().is_some_and(|due| due.claim(now)) {
                        info!(
                            "\n{}\n\n{}",
                            cluster_info.contact_info_trace(),
                            cluster_info.rpc_info_trace()
                        );
                    }
                    if contact_save.as_mut().is_some_and(|due| due.claim(now)) {
                        cluster_info.save_contact_info();
                    }
                    sleep(POLL_INTERVAL);
                }
            })
            .unwrap()
    }
}
