use {
    crate::{
        banking_stage::LikeClusterInfo,
        banking_trace::{BankingPacketBatch, BankingPacketReceiver},
        next_leader::{next_leader, next_leader_tpu_vote},
    },
    solana_client::connection_cache::ConnectionCache,
    solana_connection_cache::client_connection::ClientConnection,
    solana_perf::data_budget::DataBudget,
    solana_poh::poh_recorder::PohRecorder,
    solana_sdk::pubkey::Pubkey,
    solana_streamer::sendmmsg::batch_send,
    solana_tpu_client_next::transaction_batch::TransactionBatch,
    std::{
        iter::repeat,
        net::{SocketAddr, UdpSocket},
        sync::{Arc, RwLock},
        thread::{Builder, JoinHandle},
    },
    tokio::sync::mpsc,
};

#[derive(Clone)]
pub enum TransactionForwardClient {
    ConnectionCache(Arc<ConnectionCache>),
    TpuClientNextSender(mpsc::Sender<TransactionBatch>),
}

impl From<Arc<ConnectionCache>> for TransactionForwardClient {
    fn from(cache: Arc<ConnectionCache>) -> Self {
        TransactionForwardClient::ConnectionCache(cache)
    }
}

pub struct ForwardingStage<T: LikeClusterInfo> {
    receiver: BankingPacketReceiver,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    cluster_info: T,
    transaction_client: TransactionForwardClient,
    data_budget: DataBudget,
    udp_socket: UdpSocket,
}

impl<T: LikeClusterInfo> ForwardingStage<T> {
    pub fn spawn(
        receiver: BankingPacketReceiver,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        cluster_info: T,
        transaction_client: TransactionForwardClient,
        data_budget: DataBudget,
    ) -> JoinHandle<()> {
        let forwarding_stage = Self {
            receiver,
            poh_recorder,
            cluster_info,
            transaction_client,
            data_budget,
            udp_socket: UdpSocket::bind("0.0.0.0:0").unwrap(),
        };
        Builder::new()
            .name("solFwdStage".to_string())
            .spawn(move || forwarding_stage.run())
            .unwrap()
    }

    fn run(self) {
        while let Ok(packet_batches) = self.receiver.recv() {
            // Determine if these are vote packets or non-vote packets.
            let tpu_vote_batch = Self::is_tpu_vote(&packet_batches);

            // Get the leader and address to forward the packets to.
            let leader_address =
                if let Some((_leader, leader_address)) = self.get_leader_and_addr(tpu_vote_batch) {
                    // If unknown leader, move to next packet batch.
                    Some(leader_address)
                } else {
                    None
                };
            if leader_address.is_none() {
                if tpu_vote_batch {
                    continue;
                }
                if let TransactionForwardClient::ConnectionCache(_) = &self.transaction_client {
                    continue;
                }
            }

            self.update_data_budget();

            let packet_vec: Vec<_> = packet_batches
                .0
                .iter()
                .flat_map(|batch| batch.iter())
                .filter(|p| !p.meta().forwarded())
                .filter(|p| p.meta().is_from_staked_node())
                .filter(|p| self.data_budget.take(p.meta().size))
                .filter_map(|p| p.data(..).map(|data| data.to_vec()))
                .collect();

            if tpu_vote_batch {
                // The vote must be forwarded using only UDP.
                let pkts: Vec<_> = packet_vec
                    .into_iter()
                    .zip(repeat(leader_address.unwrap()))
                    .collect();
                let _ = batch_send(&self.udp_socket, &pkts);
            } else {
                match &self.transaction_client {
                    TransactionForwardClient::ConnectionCache(connection_cache) => {
                        let conn = connection_cache.get_connection(&leader_address.unwrap());
                        let _ = conn.send_data_batch_async(packet_vec);
                    }
                    TransactionForwardClient::TpuClientNextSender(sender) => {
                        if sender.try_send(TransactionBatch::new(packet_vec)).is_err() {
                            debug!("Failed to forward transactions because channel is full, drop them.");
                        }
                    }
                }
            }
        }
    }

    /// Get the pubkey and socket address for the leader to forward to
    fn get_leader_and_addr(&self, tpu_vote: bool) -> Option<(Pubkey, SocketAddr)> {
        if tpu_vote {
            next_leader_tpu_vote(&self.cluster_info, &self.poh_recorder)
        } else {
            match &self.transaction_client {
                TransactionForwardClient::ConnectionCache(connection_cache) => {
                    next_leader(&self.cluster_info, &self.poh_recorder, |node| {
                        node.tpu_forwards(connection_cache.protocol())
                    })
                }
                TransactionForwardClient::TpuClientNextSender(_sender) => {
                    // we don't really need to know leader explicitly here
                    None
                }
            }
        }
    }

    /// Re-fill the data budget if enough time has passed
    fn update_data_budget(&self) {
        const INTERVAL_MS: u64 = 100;
        // 12 MB outbound limit per second
        const MAX_BYTES_PER_SECOND: usize = 12_000_000;
        const MAX_BYTES_PER_INTERVAL: usize = MAX_BYTES_PER_SECOND * INTERVAL_MS as usize / 1000;
        const MAX_BYTES_BUDGET: usize = MAX_BYTES_PER_INTERVAL * 5;
        self.data_budget.update(INTERVAL_MS, |bytes| {
            std::cmp::min(
                bytes.saturating_add(MAX_BYTES_PER_INTERVAL),
                MAX_BYTES_BUDGET,
            )
        });
    }

    /// Check if `packet_batches` came from tpu_vote or tpu.
    /// Returns true if the packets are from tpu_vote, false if from tpu.
    fn is_tpu_vote(packet_batches: &BankingPacketBatch) -> bool {
        packet_batches
            .0
            .first()
            .and_then(|batch| batch.iter().next())
            .map(|packet| packet.meta().is_simple_vote_tx())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            banking_stage::test_utilities::{
                create_slow_genesis_config_with_leader, new_test_cluster_info,
            },
            sigverify::SigverifyTracerPacketStats,
        },
        crossbeam_channel::unbounded,
        solana_client::{connection_cache::ConnectionCache, rpc_client::SerializableTransaction},
        solana_gossip::cluster_info::{ClusterInfo, Node},
        solana_ledger::{blockstore::Blockstore, genesis_utils::GenesisConfigInfo},
        solana_perf::packet::PacketBatch,
        solana_poh::{poh_recorder::create_test_recorder, poh_service::PohService},
        solana_runtime::bank::Bank,
        solana_sdk::{
            hash::Hash, packet::Packet, poh_config::PohConfig, signature::Keypair, signer::Signer,
            system_transaction, transaction::VersionedTransaction,
        },
        solana_streamer::{
            nonblocking::testing_utilities::{
                setup_quic_server_with_sockets, SpawnTestServerResult, TestServerConfig,
            },
            quic::rt,
        },
        std::{
            sync::atomic::{AtomicBool, Ordering},
            time::{Duration, Instant},
        },
        tempfile::TempDir,
        tokio::time::sleep,
    };

    struct TestSetup {
        _ledger_dir: TempDir,
        blockhash: Hash,
        rent_min_balance: u64,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        exit: Arc<AtomicBool>,
        poh_service: PohService,
        cluster_info: Arc<ClusterInfo>,
        local_node: Node,
    }

    fn setup() -> TestSetup {
        let validator_keypair = Arc::new(Keypair::new());
        let genesis_config_info =
            create_slow_genesis_config_with_leader(10_000, &validator_keypair.pubkey());
        let GenesisConfigInfo { genesis_config, .. } = &genesis_config_info;

        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        let ledger_path = TempDir::new().unwrap();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.as_ref())
                .expect("Expected to be able to open database ledger"),
        );
        let poh_config = PohConfig {
            // limit tick count to avoid clearing working_bank at
            // PohRecord then PohRecorderError(MaxHeightReached) at BankingStage
            target_tick_count: Some(bank.max_tick_height() - 1),
            ..PohConfig::default()
        };

        let (exit, poh_recorder, poh_service, _entry_receiver) =
            create_test_recorder(bank, blockstore, Some(poh_config), None);

        let (local_node, cluster_info) = new_test_cluster_info(Some(validator_keypair));
        let cluster_info = Arc::new(cluster_info);

        TestSetup {
            _ledger_dir: ledger_path,
            blockhash: genesis_config.hash(),
            rent_min_balance: genesis_config.rent.minimum_balance(0),
            poh_recorder,
            exit,
            poh_service,
            cluster_info,
            local_node,
        }
    }

    async fn check_all_received(
        socket: UdpSocket,
        expected_num_packets: usize,
        expected_packet_size: usize,
        expected_blockhash: &Hash,
    ) {
        let SpawnTestServerResult {
            join_handle,
            exit,
            receiver,
            server_address: _,
            stats: _,
        } = setup_quic_server_with_sockets(vec![socket], None, TestServerConfig::default());

        let now = Instant::now();
        let mut total_packets = 0;
        while now.elapsed().as_secs() < 5 {
            if let Ok(packets) = receiver.try_recv() {
                total_packets += packets.len();
                for packet in packets.iter() {
                    assert_eq!(packet.meta().size, expected_packet_size);
                    let tx: VersionedTransaction = packet.deserialize_slice(..).unwrap();
                    assert_eq!(
                        tx.get_recent_blockhash(),
                        expected_blockhash,
                        "Unexpected blockhash, tx: {tx:?}, expected blockhash: {expected_blockhash}."
                    );
                }
            } else {
                sleep(Duration::from_millis(100)).await;
            }
            if total_packets >= expected_num_packets {
                break;
            }
        }
        assert_eq!(total_packets, expected_num_packets);

        exit.store(true, Ordering::Relaxed);
        join_handle.await.unwrap();
    }

    #[test]
    fn test_forwarder_budget() {
        let TestSetup {
            blockhash,
            rent_min_balance,
            poh_recorder,
            exit,
            poh_service,
            cluster_info,
            local_node,
            ..
        } = setup();

        // Create `PacketBatch` with 1 unprocessed packet
        let tx = system_transaction::transfer(
            &Keypair::new(),
            &solana_sdk::pubkey::new_rand(),
            rent_min_balance,
            blockhash,
        );
        let mut packet = Packet::from_data(None, tx).unwrap();
        // unstaked transactions will not be forwarded
        packet.meta_mut().set_from_staked_node(true);
        let expected_packet_size = packet.meta().size;
        let packet_batches = vec![PacketBatch::new(vec![packet])];

        let test_cases = vec![
            ("budget-restricted", DataBudget::restricted(), 0),
            ("budget-available", DataBudget::default(), 1),
        ];
        let runtime = rt("solQuicTestRt".to_string());
        let connection_cache = Arc::new(ConnectionCache::new("connection_cache_test"));
        for (_name, data_budget, expected_num_forwarded) in test_cases {
            let (forward_stage_sender, forward_stage_receiver) = unbounded();
            let forwarding_stage = ForwardingStage::spawn(
                forward_stage_receiver,
                poh_recorder.clone(),
                cluster_info.clone(),
                connection_cache.clone().into(),
                data_budget,
            );
            let tracer_packet_stats_to_send = SigverifyTracerPacketStats::default();
            let banking_packet_batch = BankingPacketBatch::new((
                packet_batches.clone(),
                Some(tracer_packet_stats_to_send),
            ));
            forward_stage_sender.send(banking_packet_batch).unwrap();

            let recv_socket = &local_node.sockets.tpu_forwards_quic[0];
            runtime.block_on(check_all_received(
                (*recv_socket).try_clone().unwrap(),
                expected_num_forwarded,
                expected_packet_size,
                &blockhash,
            ));
            drop(forward_stage_sender);
            forwarding_stage.join().unwrap();
        }

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }
}
