// Connect to future leaders with some jitter so the quic connection is warm
// by the time we need it.

use {
    rand::{thread_rng, Rng},
    solana_client::connection_cache::{ConnectionCache, Protocol},
    solana_connection_cache::client_connection::ClientConnection as TpuConnection,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfoQuery},
    solana_poh::poh_recorder::PohRecorder,
    solana_pubkey::Pubkey,
    std::{
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::Duration,
    },
};

pub struct WarmQuicCacheService {
    thread_hdl: JoinHandle<()>,
}

// ~50 seconds
const CACHE_OFFSET_SLOT: i64 = 100;
const CACHE_JITTER_SLOT: i64 = 20;

impl WarmQuicCacheService {
    fn warmup_connection(
        cache: Option<&ConnectionCache>,
        cluster_info: &ClusterInfo,
        leader_pubkey: &Pubkey,
        contact_info_selector: impl ContactInfoQuery<Option<SocketAddr>>,
        log_context: &str,
    ) {
        if let Some(connection_cache) = cache {
            if let Some(Some(addr)) =
                cluster_info.lookup_contact_info(leader_pubkey, contact_info_selector)
            {
                let conn = connection_cache.get_connection(&addr);
                if let Err(err) = conn.send_data(&[]) {
                    warn!(
                        "Failed to warmup QUIC connection to the leader {leader_pubkey:?} at \
                         {addr:?}, Context: {log_context}, Error: {err:?}"
                    );
                }
            }
        }
    }

    pub fn new(
        tpu_connection_cache: Option<Arc<ConnectionCache>>,
        vote_connection_cache: Option<Arc<ConnectionCache>>,
        cluster_info: Arc<ClusterInfo>,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        assert!(matches!(
            tpu_connection_cache.as_deref(),
            None | Some(ConnectionCache::Quic(_))
        ));
        assert!(matches!(
            vote_connection_cache.as_deref(),
            None | Some(ConnectionCache::Quic(_))
        ));
        let thread_hdl = Builder::new()
            .name("solWarmQuicSvc".to_string())
            .spawn(move || {
                let slot_jitter = thread_rng().gen_range(-CACHE_JITTER_SLOT..CACHE_JITTER_SLOT);
                let mut maybe_last_leader = None;
                while !exit.load(Ordering::Relaxed) {
                    let leader_pubkey = poh_recorder
                        .read()
                        .unwrap()
                        .leader_after_n_slots((CACHE_OFFSET_SLOT + slot_jitter) as u64);
                    if let Some(leader_pubkey) = leader_pubkey {
                        if maybe_last_leader != Some(leader_pubkey) {
                            maybe_last_leader = Some(leader_pubkey);
                            // Warm cache for regular transactions
                            Self::warmup_connection(
                                tpu_connection_cache.as_deref(),
                                &cluster_info,
                                &leader_pubkey,
                                |node| node.tpu(Protocol::QUIC),
                                "tpu",
                            );
                            // Warm cache for vote
                            Self::warmup_connection(
                                vote_connection_cache.as_deref(),
                                &cluster_info,
                                &leader_pubkey,
                                |node| node.tpu_vote(Protocol::QUIC),
                                "vote",
                            );
                        }
                    }
                    sleep(Duration::from_millis(200));
                }
            })
            .unwrap();
        Self { thread_hdl }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
