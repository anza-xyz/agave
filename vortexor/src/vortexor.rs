use {
    crossbeam_channel::Sender,
    solana_perf::packet::PacketBatch,
    solana_sdk::{quic::NotifyKeyUpdate, signature::Keypair},
    solana_streamer::{
        quic::{spawn_server_multi, EndpointKeyUpdater},
        streamer::StakedNodes,
    },
    std::{
        net::UdpSocket,
        sync::{atomic::AtomicBool, Arc, Mutex, RwLock},
        thread::{self, JoinHandle},
        time::Duration,
    },
};

pub const MAX_QUIC_CONNECTIONS_PER_PEER: usize = 8;
pub const NUM_QUIC_ENDPOINTS: usize = 8;

pub struct TpuSockets {
    pub tpu_quic: Vec<UdpSocket>,
    pub tpu_quic_fwd: Vec<UdpSocket>,
}

pub struct TpuStreamerConfig {
    pub tpu_thread_name: &'static str,
    pub tpu_metrics_name: &'static str,
    pub tpu_fwd_thread_name: &'static str,
    pub tpu_fwd_metrics_name: &'static str,
    pub max_connections_per_peer: usize,
    pub max_staked_connections: usize,
    pub max_unstaked_connections: usize,
    pub max_streams_per_ms: u64,
    pub max_connections_per_ipaddr_per_min: u64,
    pub wait_for_chunk_timeout: Duration,
    pub sender_coalesce_duration: Duration,
}

pub struct Vortexor {
    thread_handles: Vec<JoinHandle<()>>,
    key_update_notifier: Arc<KeyUpdateNotifier>,
}

struct KeyUpdateNotifier {
    key_updaters: Mutex<Vec<Arc<EndpointKeyUpdater>>>,
}

impl KeyUpdateNotifier {
    fn new(key_updaters: Vec<Arc<EndpointKeyUpdater>>) -> Self {
        Self {
            key_updaters: Mutex::new(key_updaters),
        }
    }
}

impl NotifyKeyUpdate for KeyUpdateNotifier {
    fn update_key(&self, key: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let updaters = self.key_updaters.lock().unwrap();
        for updater in updaters.iter() {
            updater.update_key(key)?
        }
        Ok(())
    }
}

impl Vortexor {
    /// Create a new TPU Vortexor
    pub fn new(
        keypair: &Keypair,
        tpu_sockets: TpuSockets,
        tpu_sender: Sender<PacketBatch>,
        tpu_fwd_sender: Sender<PacketBatch>,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        config: TpuStreamerConfig,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let TpuSockets {
            tpu_quic,
            tpu_quic_fwd,
        } = tpu_sockets;

        let TpuStreamerConfig {
            tpu_thread_name,
            tpu_metrics_name,
            tpu_fwd_thread_name,
            tpu_fwd_metrics_name,
            max_connections_per_peer,
            max_staked_connections,
            max_unstaked_connections,
            max_streams_per_ms,
            max_connections_per_ipaddr_per_min,
            wait_for_chunk_timeout,
            sender_coalesce_duration,
        } = config;

        let tpu_result = spawn_server_multi(
            tpu_thread_name,
            tpu_metrics_name,
            tpu_quic,
            keypair,
            tpu_sender.clone(),
            exit.clone(),
            max_connections_per_peer,
            staked_nodes.clone(),
            max_staked_connections,
            max_unstaked_connections,
            max_streams_per_ms,
            max_connections_per_ipaddr_per_min,
            wait_for_chunk_timeout,
            sender_coalesce_duration,
        )
        .unwrap();

        let tpu_fwd_result = spawn_server_multi(
            tpu_fwd_thread_name,
            tpu_fwd_metrics_name,
            tpu_quic_fwd,
            keypair,
            tpu_fwd_sender,
            exit.clone(),
            max_connections_per_peer,
            staked_nodes.clone(),
            max_staked_connections,
            max_unstaked_connections,
            max_streams_per_ms,
            max_connections_per_ipaddr_per_min,
            wait_for_chunk_timeout,
            sender_coalesce_duration,
        )
        .unwrap();

        Self {
            thread_handles: vec![tpu_result.thread, tpu_fwd_result.thread],
            key_update_notifier: Arc::new(KeyUpdateNotifier::new(vec![
                tpu_result.key_updater,
                tpu_fwd_result.key_updater,
            ])),
        }
    }

    pub fn get_key_update_notifier(&self) -> Arc<dyn NotifyKeyUpdate + Sync + Send> {
        self.key_update_notifier.clone()
    }

    pub fn join(self) -> thread::Result<()> {
        for t in self.thread_handles {
            t.join()?
        }
        Ok(())
    }
}
