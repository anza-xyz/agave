use {
    crossbeam_channel::{Receiver, Sender},
    solana_core::{
        banking_trace::TracedSender, sigverify::TransactionSigVerifier,
        sigverify_stage::SigVerifyStage,
    },
    solana_net_utils::{bind_in_range_with_config, bind_more_with_config, SocketConfig},
    solana_perf::packet::PacketBatch,
    solana_sdk::{quic::NotifyKeyUpdate, signature::Keypair},
    solana_streamer::{
        nonblocking::quic::DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
        quic::{spawn_server_multi, EndpointKeyUpdater, QuicServerParams},
        streamer::StakedNodes,
    },
    std::{
        net::UdpSocket,
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
        num_quic_endpoints: u64,
    ) -> TpuSockets {
        let quic_config = SocketConfig::default().reuseport(true);

        let (_, tpu_quic) =
            bind_in_range_with_config(bind_address, dynamic_port_range, quic_config)
                .expect("expected bind to succeed");

        let tpu_quic_port = tpu_quic.local_addr().unwrap().port();
        let tpu_quic = bind_more_with_config(
            tpu_quic,
            num_quic_endpoints.try_into().unwrap(),
            quic_config,
        )
        .unwrap();

        let (_, tpu_quic_fwd) = bind_in_range_with_config(
            bind_address,
            (tpu_quic_port.saturating_add(1), dynamic_port_range.1),
            quic_config,
        )
        .expect("expected bind to succeed");

        let tpu_quic_fwd = bind_more_with_config(
            tpu_quic_fwd,
            num_quic_endpoints.try_into().unwrap(),
            quic_config,
        )
        .unwrap();

        TpuSockets {
            tpu_quic,
            tpu_quic_fwd,
        }
    }

    pub fn create_sigverify_stage(
        tpu_receiver: Receiver<solana_perf::packet::PacketBatch>,
        non_vote_sender: TracedSender,
    ) -> SigVerifyStage {
        let verifier = TransactionSigVerifier::new(non_vote_sender);
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
        max_connections_per_peer: u64,
        max_tpu_staked_connections: u64,
        max_tpu_unstaked_connections: u64,
        max_fwd_staked_connections: u64,
        max_fwd_unstaked_connections: u64,
        max_streams_per_ms: u64,
        max_connections_per_ipaddr_per_min: u64,
        tpu_coalesce: Duration,
        identity_keypair: &Keypair,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let mut quic_server_params = QuicServerParams {
            max_connections_per_peer: max_connections_per_peer.try_into().unwrap(),
            max_staked_connections: max_tpu_staked_connections.try_into().unwrap(),
            max_unstaked_connections: max_tpu_unstaked_connections.try_into().unwrap(),
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
        quic_server_params.max_staked_connections = max_fwd_staked_connections.try_into().unwrap();
        quic_server_params.max_unstaked_connections =
            max_fwd_unstaked_connections.try_into().unwrap();
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
