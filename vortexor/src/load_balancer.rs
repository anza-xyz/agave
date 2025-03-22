//! Module responsible for interacting with RPC services.
//! Adapted from jito-replayer code.

use {
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender},
    dashmap::DashMap,
    log::{error, info},
    solana_client::{pubsub_client::PubsubClient, rpc_client::RpcClient},
    solana_metrics::{datapoint_error, datapoint_info},
    solana_sdk::{
        clock::Slot,
        commitment_config::{CommitmentConfig, CommitmentLevel},
    },
    std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

/// The RpcLoadBalancer can support providing a RpcClient which has the most
/// up-to-date slot information among a set of configured RPC servers.
/// This service starts threads which will subscribe to the slot events
/// using the corresponding web socket for a RPC server. This mechansim can
/// load balance the RPC calls to multiple RPC servers.
pub struct RpcLoadBalancer {
    /// (ws_url, slot)
    server_to_slot: Arc<DashMap<String, Slot>>,
    /// (rpc_url, client)
    server_to_rpc_client: DashMap<String, Arc<RpcClient>>,
    subscription_threads: Vec<JoinHandle<()>>,
}

impl RpcLoadBalancer {
    const DISCONNECT_WEBSOCKET_TIMEOUT: Duration = Duration::from_secs(30);
    const RPC_TIMEOUT: Duration = Duration::from_secs(15);
    pub const SLOT_QUEUE_CAPACITY: usize = 100;

    /// Start a Load Balancer service given a list of RPC servers. Each RPC server
    /// must have the (RPC URL and corresponding Websocket URL) set.
    /// This create the RpcClients and create web socket connections to the corresponding
    /// RPC server and watch for the slot events.
    pub fn new(
        servers: &[(String, String)], /* http rpc url, ws url */
        exit: &Arc<AtomicBool>,
    ) -> (RpcLoadBalancer, Receiver<Slot>) {
        let server_to_slot = Arc::new(DashMap::from_iter(
            servers.iter().map(|(_, ws)| (ws.clone(), 0)),
        ));

        let server_to_rpc_client = DashMap::from_iter(servers.iter().map(|(rpc_url, ws)| {
            // warm up the connection
            let rpc_client = Arc::new(RpcClient::new_with_timeout_and_commitment(
                rpc_url,
                Self::RPC_TIMEOUT,
                CommitmentConfig {
                    commitment: CommitmentLevel::Processed,
                },
            ));
            if let Err(e) = rpc_client.get_slot() {
                error!("error warming up rpc: {rpc_url}. error: {e}");
            }
            // store as ws instead of http so we can lookup by furthest ahead ws subscription
            (ws.clone(), rpc_client)
        }));

        // sender tracked as health_manager-channel_stats.slot_sender_len
        let (slot_sender, slot_receiver) = crossbeam_channel::bounded(Self::SLOT_QUEUE_CAPACITY);
        let subscription_threads =
            Self::start_subscription_threads(servers, server_to_slot.clone(), slot_sender, exit);
        (
            RpcLoadBalancer {
                server_to_slot,
                server_to_rpc_client,
                subscription_threads,
            },
            slot_receiver,
        )
    }

    /// Start threads to watch for slot events on the RPC servers and update
    /// the server_to_slot map.
    fn start_subscription_threads(
        servers: &[(String, String)],
        server_to_slot: Arc<DashMap<String, Slot>>,
        slot_sender: Sender<Slot>,
        exit: &Arc<AtomicBool>,
    ) -> Vec<JoinHandle<()>> {
        info!("start_subscription_threads for servers: {servers:?}");
        let highest_slot = Arc::new(AtomicU64::default());

        servers
            .iter()
            .map(|(_, websocket_url)| {
                let ws_url_no_token = websocket_url
                    .split('/')
                    .nth(2)
                    .unwrap_or_default()
                    .to_string();
                let exit = exit.clone();
                let websocket_url = websocket_url.clone();
                let server_to_slot = server_to_slot.clone();
                let slot_sender = slot_sender.clone();
                let highest_slot = highest_slot.clone();

                Builder::new()
                    .name(format!("load_balancer_subscription_thread-{ws_url_no_token}"))
                    .spawn(move || {
                        while !exit.load(Ordering::Relaxed) {
                            info!("running slot_subscribe() with url: {websocket_url}");
                            let mut last_slot_update = Instant::now();

                            match PubsubClient::slot_subscribe(&websocket_url) {
                                Ok((_subscription, receiver)) => {
                                    while !exit.load(Ordering::Relaxed) {
                                        match receiver.recv_timeout(Duration::from_millis(100))
                                        {
                                            Ok(slot) => {
                                                last_slot_update = Instant::now();

                                                server_to_slot
                                                    .insert(websocket_url.clone(), slot.slot);
                                                datapoint_info!(
                                                        "rpc_load_balancer-slot_count",
                                                        "url" => ws_url_no_token,
                                                        ("slot", slot.slot, i64)
                                                );

                                                {
                                                    let old_slot = highest_slot.fetch_max(slot.slot, Ordering::Relaxed);
                                                    if slot.slot > old_slot {
                                                        if let Err(e) = slot_sender.send(slot.slot)
                                                        {
                                                            error!("error sending slot: {e}");
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                            Err(RecvTimeoutError::Timeout) => {
                                                // RPC servers occasionally stop sending slot updates and never recover.
                                                // If enough time has passed, attempt to recover by forcing a new connection
                                                if last_slot_update.elapsed() >= Self::DISCONNECT_WEBSOCKET_TIMEOUT
                                                {
                                                    datapoint_error!(
                                                        "rpc_load_balancer-force_disconnect",
                                                        "url" => ws_url_no_token,
                                                        ("event", 1, i64)
                                                    );
                                                    break;
                                                }
                                            }
                                            Err(RecvTimeoutError::Disconnected) => {
                                                info!("slot subscribe disconnected. url: {ws_url_no_token}");
                                                break;
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        "slot subscription error client: {ws_url_no_token}, error: {e:?}"
                                    );
                                }
                            }

                            sleep(Duration::from_secs(1));
                        }
                    })
                    .unwrap()
            })
            .collect()
    }

    /// Returns the server with the highest slot URL
    pub fn rpc_client(&self) -> Arc<RpcClient> {
        let (highest_server, _) = self.get_highest_slot();

        self.server_to_rpc_client
            .get(&highest_server)
            .unwrap()
            .value()
            .to_owned()
    }

    /// Return the server's WebSocket URL which as the most update slot
    pub fn get_highest_slot(&self) -> (String, Slot) {
        let multi = self
            .server_to_slot
            .iter()
            .max_by(|lhs, rhs| lhs.value().cmp(rhs.value()))
            .unwrap();
        let (server, slot) = multi.pair();
        (server.to_string(), *slot)
    }

    pub fn join(self) -> thread::Result<()> {
        for s in self.subscription_threads {
            s.join()?;
        }
        Ok(())
    }
}
