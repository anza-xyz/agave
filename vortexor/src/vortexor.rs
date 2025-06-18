use {
    crossbeam_channel::{Receiver, Sender},
    solana_core::{
        banking_trace::TracedSender, sigverify::TransactionSigVerifier,
        sigverify_stage::SigVerifyStage,
    },
    solana_keypair::Keypair,
    solana_net_utils::sockets::{
        multi_bind_in_range_with_config, SocketConfiguration as SocketConfig,
    },
    solana_perf::packet::PacketBatch,
    solana_quic_definitions::NotifyKeyUpdate,
    solana_streamer::{
        nonblocking::quic::DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
        quic::{spawn_server_multi, EndpointKeyUpdater, QuicServerParams},
        streamer::StakedNodes,
    },
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{atomic::AtomicBool, Arc, Mutex, RwLock},
        thread::{self, JoinHandle},
        time::Duration,
    },
};

pub struct TpuSockets {
    pub tpu_quic: Vec<UdpSocket>,
    pub tpu_quic_fwd: Vec<UdpSocket>,
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
    pub fn create_tpu_sockets(
        bind_address: std::net::IpAddr,
        dynamic_port_range: (u16, u16),
        tpu_address: Option<SocketAddr>,
        tpu_forward_address: Option<SocketAddr>,
        num_quic_endpoints: usize,
    ) -> TpuSockets {
        let quic_config = SocketConfig::default();

        let tpu_quic = bind_sockets(
            bind_address,
            dynamic_port_range,
            tpu_address,
            num_quic_endpoints,
            quic_config,
        );

        let tpu_quic_fwd = bind_sockets(
            bind_address,
            dynamic_port_range,
            tpu_forward_address,
            num_quic_endpoints,
            quic_config,
        );

        TpuSockets {
            tpu_quic,
            tpu_quic_fwd,
        }
    }

    pub fn create_sigverify_stage(
        tpu_receiver: Receiver<PacketBatch>,
        non_vote_sender: TracedSender,
    ) -> SigVerifyStage {
        let verifier = TransactionSigVerifier::new(non_vote_sender, None);
        SigVerifyStage::new(
            tpu_receiver,
            verifier,
            "solSigVtxTpu",
            "tpu-vortexor-verifier",
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_vortexor(
        tpu_sockets: TpuSockets,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        tpu_sender: Sender<PacketBatch>,
        tpu_fwd_sender: Sender<PacketBatch>,
        max_connections_per_peer: usize,
        max_tpu_staked_connections: usize,
        max_tpu_unstaked_connections: usize,
        max_fwd_staked_connections: usize,
        max_fwd_unstaked_connections: usize,
        max_streams_per_ms: u64,
        max_connections_per_ipaddr_per_min: u64,
        tpu_coalesce: Duration,
        identity_keypair: &Keypair,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let mut quic_server_params = QuicServerParams {
            max_connections_per_peer,
            max_staked_connections: max_tpu_staked_connections,
            max_unstaked_connections: max_tpu_unstaked_connections,
            max_streams_per_ms,
            max_connections_per_ipaddr_per_min,
            wait_for_chunk_timeout: DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
            coalesce: tpu_coalesce,
            ..Default::default()
        };

        let TpuSockets {
            tpu_quic,
            tpu_quic_fwd,
        } = tpu_sockets;

        let tpu_result = spawn_server_multi(
            "solVtxTpu",
            "quic_vortexor_tpu",
            tpu_quic,
            identity_keypair,
            tpu_sender.clone(),
            exit.clone(),
            staked_nodes.clone(),
            quic_server_params.clone(),
        )
        .unwrap();

        // Fot TPU forward -- we disallow unstaked connections. Allocate all connection resources
        // for staked connections:
        quic_server_params.max_staked_connections = max_fwd_staked_connections;
        quic_server_params.max_unstaked_connections = max_fwd_unstaked_connections;
        let tpu_fwd_result = spawn_server_multi(
            "solVtxTpuFwd",
            "quic_vortexor_tpu_forwards",
            tpu_quic_fwd,
            identity_keypair,
            tpu_fwd_sender,
            exit.clone(),
            staked_nodes.clone(),
            quic_server_params,
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

/// Binds the sockets to the specified address and port range if address is Some.
/// If the address is None, it binds to the specified bind_address and port range.
fn bind_sockets(
    bind_address: std::net::IpAddr,
    port_range: (u16, u16),
    address: Option<SocketAddr>,
    num_quic_endpoints: usize,
    quic_config: SocketConfig,
) -> Vec<UdpSocket> {
    let (bind_address, port_range) = address
        .map(|addr| (addr.ip(), (addr.port(), addr.port().saturating_add(1))))
        .unwrap_or((bind_address, port_range));

    let (_, sockets) =
        multi_bind_in_range_with_config(bind_address, port_range, quic_config, num_quic_endpoints)
            .expect("expected bind to succeed");
    sockets
}
