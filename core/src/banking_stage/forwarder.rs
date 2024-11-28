use {
    super::{
        forward_packet_batches_by_accounts::ForwardPacketBatchesByAccounts,
        leader_slot_metrics::LeaderSlotMetricsTracker,
        unprocessed_transaction_storage::UnprocessedTransactionStorage, BankingStageStats,
        ForwardOption,
    },
    crate::{
        banking_stage::immutable_deserialized_packet::ImmutableDeserializedPacket,
        forwarder_client::ForwarderClient, tracer_packet_stats::TracerPacketStats,
    },
    solana_feature_set::FeatureSet,
    solana_perf::{data_budget::DataBudget, packet::Packet},
    solana_runtime::bank_forks::BankForks,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk::{pubkey::Pubkey, transport::TransportError},
    solana_streamer::sendmmsg::{batch_send, SendPktsError},
    solana_svm_transaction::svm_message::SVMMessage,
    std::{
        iter::repeat,
        net::UdpSocket,
        sync::{atomic::Ordering, Arc, RwLock},
    },
};

pub(crate) struct Forwarder<Client: ForwarderClient> {
    bank_forks: Arc<RwLock<BankForks>>,
    socket: UdpSocket,
    client: Client,
    data_budget: Arc<DataBudget>,
    forward_packet_batches_by_accounts: ForwardPacketBatchesByAccounts,
}

impl<Client: ForwarderClient> Forwarder<Client> {
    pub fn new(
        client: Client,
        bank_forks: Arc<RwLock<BankForks>>,
        data_budget: Arc<DataBudget>,
    ) -> Self {
        Self {
            bank_forks,
            socket: UdpSocket::bind("0.0.0.0:0").unwrap(),
            client,
            data_budget,
            forward_packet_batches_by_accounts:
                ForwardPacketBatchesByAccounts::new_with_default_batch_limits(),
        }
    }

    pub fn clear_batches(&mut self) {
        self.forward_packet_batches_by_accounts.reset();
    }

    pub fn try_add_packet(
        &mut self,
        sanitized_transaction: &RuntimeTransaction<impl SVMMessage>,
        immutable_packet: Arc<ImmutableDeserializedPacket>,
        feature_set: &FeatureSet,
    ) -> bool {
        self.forward_packet_batches_by_accounts.try_add_packet(
            sanitized_transaction,
            immutable_packet,
            feature_set,
        )
    }

    pub fn forward_batched_packets(&self, forward_option: &ForwardOption) {
        self.forward_packet_batches_by_accounts
            .iter_batches()
            .filter(|&batch| !batch.is_empty())
            .for_each(|forwardable_batch| {
                let _ = self
                    .forward_packets(forward_option, forwardable_batch.get_forwardable_packets());
            });
    }

    /// This function is exclusively used by multi-iterator banking threads to handle forwarding
    /// logic per thread. Central scheduler Controller uses try_add_packet() ... forward_batched_packets()
    /// to handle forwarding slight differently.
    pub fn handle_forwarding(
        &mut self,
        unprocessed_transaction_storage: &mut UnprocessedTransactionStorage,
        hold: bool,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        banking_stage_stats: &BankingStageStats,
        tracer_packet_stats: &mut TracerPacketStats,
    ) {
        let forward_option = unprocessed_transaction_storage.forward_option();

        // get current working bank from bank_forks, use it to sanitize transaction and
        // load all accounts from address loader;
        let current_bank = self.bank_forks.read().unwrap().working_bank();

        // if we have crossed an epoch boundary, recache any state
        unprocessed_transaction_storage.cache_epoch_boundary_info(&current_bank);

        // sanitize and filter packets that are no longer valid (could be too old, a duplicate of something
        // already processed), then add to forwarding buffer.
        let filter_forwarding_result = unprocessed_transaction_storage
            .filter_forwardable_packets_and_add_batches(
                current_bank,
                &mut self.forward_packet_batches_by_accounts,
            );
        slot_metrics_tracker.increment_transactions_from_packets_us(
            filter_forwarding_result.total_packet_conversion_us,
        );
        banking_stage_stats.packet_conversion_elapsed.fetch_add(
            filter_forwarding_result.total_packet_conversion_us,
            Ordering::Relaxed,
        );
        banking_stage_stats
            .filter_pending_packets_elapsed
            .fetch_add(
                filter_forwarding_result.total_filter_packets_us,
                Ordering::Relaxed,
            );
        banking_stage_stats.dropped_forward_packets_count.fetch_add(
            filter_forwarding_result.total_dropped_packets,
            Ordering::Relaxed,
        );

        self.forward_packet_batches_by_accounts
            .iter_batches()
            .filter(|&batch| !batch.is_empty())
            .for_each(|forward_batch| {
                slot_metrics_tracker.increment_forwardable_batches_count(1);

                let batched_forwardable_packets_count = forward_batch.len();
                let (_forward_result, successful_forwarded_packets_count, leader_pubkey) = self
                    .forward_buffered_packets(
                        &forward_option,
                        forward_batch.get_forwardable_packets(),
                        banking_stage_stats,
                    );

                if let Some(leader_pubkey) = leader_pubkey {
                    tracer_packet_stats.increment_total_forwardable_tracer_packets(
                        filter_forwarding_result.total_forwardable_tracer_packets,
                        leader_pubkey,
                    );
                }
                let failed_forwarded_packets_count = batched_forwardable_packets_count
                    .saturating_sub(successful_forwarded_packets_count);

                if failed_forwarded_packets_count > 0 {
                    slot_metrics_tracker.increment_failed_forwarded_packets_count(
                        failed_forwarded_packets_count as u64,
                    );
                    slot_metrics_tracker.increment_packet_batch_forward_failure_count(1);
                }

                if successful_forwarded_packets_count > 0 {
                    slot_metrics_tracker.increment_successful_forwarded_packets_count(
                        successful_forwarded_packets_count as u64,
                    );
                }
            });
        self.clear_batches();

        if !hold {
            slot_metrics_tracker.increment_cleared_from_buffer_after_forward_count(
                filter_forwarding_result.total_forwardable_packets as u64,
            );
            tracer_packet_stats.increment_total_cleared_from_buffer_after_forward(
                filter_forwarding_result.total_tracer_packets_in_buffer,
            );
            unprocessed_transaction_storage.clear_forwarded_packets();
        }
    }

    /// Forwards all valid, unprocessed packets in the iterator, up to a rate limit.
    /// Returns whether forwarding succeeded, the number of attempted forwarded packets
    /// if any, the time spent forwarding in us, and the leader pubkey if any.
    fn forward_packets<'a>(
        &self,
        forward_option: &ForwardOption,
        forwardable_packets: impl Iterator<Item = &'a Packet>,
    ) -> (
        std::result::Result<(), TransportError>,
        usize,
        Option<Pubkey>,
    ) {
        match forward_option {
            ForwardOption::NotForward => {
                return (Ok(()), 0, None);
            }
            ForwardOption::ForwardTransaction => {
                let Some((leader_pubkey, _)) = self.client.get_next_leader() else {
                    return (Ok(()), 0, None);
                };
                self.update_data_budget();
                let packet_vec: Vec<_> = forwardable_packets
                    .filter(|p| !p.meta().forwarded())
                    .filter(|p| p.meta().is_from_staked_node())
                    .filter(|p| self.data_budget.take(p.meta().size))
                    .filter_map(|p| p.data(..).map(|data| data.to_vec()))
                    .collect();
                let packet_vec_len = packet_vec.len();
                if packet_vec_len == 0 {
                    return (Ok(()), 0, None);
                }
                // TODO: see https://github.com/solana-labs/solana/issues/23819
                // fix this so returns the correct number of succeeded packets
                // when there's an error sending the batch. This was left as-is for now
                // in favor of shipping Quic support, which was considered higher-priority
                self.client.send_transaction_batch(packet_vec);
                (Ok(()), packet_vec_len, Some(leader_pubkey))
            }
            ForwardOption::ForwardTpuVote => {
                let Some((leader_pubkey, addr)) = self.client.get_next_leader_vote() else {
                    return (Ok(()), 0, None);
                };
                self.update_data_budget();
                let filtered_forwardable_packets = forwardable_packets
                    .filter(|p| !p.meta().forwarded())
                    .filter(|p| p.meta().is_from_staked_node())
                    .filter(|p| self.data_budget.take(p.meta().size))
                    .filter_map(|p| p.data(..).map(|data| data.to_vec()));
                // The vote must be forwarded using only UDP.
                let pkts: Vec<_> = filtered_forwardable_packets.zip(repeat(addr)).collect();
                if pkts.is_empty() {
                    return (Ok(()), 0, Some(leader_pubkey));
                }
                match batch_send(&self.socket, &pkts) {
                    Ok(()) => (Ok(()), pkts.len(), Some(leader_pubkey)),
                    Err(error) => {
                        let SendPktsError::IoError(error, num_failed) = error;
                        (
                            Err(TransportError::Custom(format!("{error:?}"))),
                            pkts.len() - num_failed,
                            Some(leader_pubkey),
                        )
                    }
                }
            }
        }
    }

    /// Forwards all valid, unprocessed packets in the buffer, up to a rate limit. Returns
    /// the number of successfully forwarded packets in second part of tuple
    fn forward_buffered_packets<'a>(
        &self,
        forward_option: &ForwardOption,
        forwardable_packets: impl Iterator<Item = &'a Packet>,
        banking_stage_stats: &BankingStageStats,
    ) -> (
        std::result::Result<(), TransportError>,
        usize,
        Option<Pubkey>,
    ) {
        let (res, num_packets, leader_pubkey) =
            self.forward_packets(forward_option, forwardable_packets);
        if let Err(ref err) = res {
            warn!("failed to forward packets: {err}");
        }

        if num_packets > 0 {
            if let ForwardOption::ForwardTpuVote = forward_option {
                banking_stage_stats
                    .forwarded_vote_count
                    .fetch_add(num_packets, Ordering::Relaxed);
            } else {
                banking_stage_stats
                    .forwarded_transaction_count
                    .fetch_add(num_packets, Ordering::Relaxed);
            }
        }

        (res, num_packets, leader_pubkey)
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
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            tests::{create_slow_genesis_config_with_leader, new_test_cluster_info},
            unprocessed_packet_batches::{DeserializedPacket, UnprocessedPacketBatches},
            unprocessed_transaction_storage::ThreadType,
            ForwarderConnectionCacheClient, ForwarderLeaderUpdater,
        },
        solana_client::{connection_cache::ConnectionCache, rpc_client::SerializableTransaction},
        solana_gossip::cluster_info::{ClusterInfo, Node},
        solana_ledger::{blockstore::Blockstore, genesis_utils::GenesisConfigInfo},
        solana_perf::packet::PacketFlags,
        solana_poh::{
            poh_recorder::{create_test_recorder, PohRecorder},
            poh_service::PohService,
        },
        solana_runtime::bank::Bank,
        solana_sdk::{
            hash::Hash, poh_config::PohConfig, signature::Keypair, signer::Signer,
            system_transaction, transaction::VersionedTransaction,
        },
        solana_streamer::{
            nonblocking::testing_utilities::{
                setup_quic_server_with_sockets, SpawnTestServerResult, TestServerConfig,
            },
            quic::rt,
        },
        std::{
            sync::atomic::AtomicBool,
            time::{Duration, Instant},
        },
        tempfile::TempDir,
        tokio::time::sleep,
    };

    struct TestSetup {
        _ledger_dir: TempDir,
        blockhash: Hash,
        rent_min_balance: u64,

        bank_forks: Arc<RwLock<BankForks>>,
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

        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(genesis_config);

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

            bank_forks,
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
            bank_forks,
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
        let deserialized_packet = DeserializedPacket::new(packet).unwrap();

        let test_cases = vec![
            ("budget-restricted", DataBudget::restricted(), 0),
            ("budget-available", DataBudget::default(), 1),
        ];
        let runtime = rt("solQuicTestRt".to_string());
        for (_name, data_budget, expected_num_forwarded) in test_cases {
            let leader_updater =
                ForwarderLeaderUpdater::new(cluster_info.clone(), poh_recorder.clone());
            let client = ForwarderConnectionCacheClient::new(
                Arc::new(ConnectionCache::new("connection_cache_test")),
                leader_updater,
            );
            let mut forwarder = Forwarder::new(client, bank_forks.clone(), Arc::new(data_budget));
            let unprocessed_packet_batches: UnprocessedPacketBatches =
                UnprocessedPacketBatches::from_iter(
                    vec![deserialized_packet.clone()].into_iter(),
                    1,
                );
            let stats = BankingStageStats::default();
            forwarder.handle_forwarding(
                &mut UnprocessedTransactionStorage::new_transaction_storage(
                    unprocessed_packet_batches,
                    ThreadType::Transactions,
                ),
                true,
                &mut LeaderSlotMetricsTracker::new(0),
                &stats,
                &mut TracerPacketStats::new(0),
            );

            let recv_socket = &local_node.sockets.tpu_forwards_quic[0];
            runtime.block_on(check_all_received(
                (*recv_socket).try_clone().unwrap(),
                expected_num_forwarded,
                expected_packet_size,
                &blockhash,
            ));
        }

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }

    #[test]
    fn test_handle_forwarding() {
        let TestSetup {
            blockhash,
            rent_min_balance,
            bank_forks,
            poh_recorder,
            exit,
            poh_service,
            cluster_info,
            local_node,
            ..
        } = setup();

        let keypair = Keypair::new();
        let pubkey = solana_sdk::pubkey::new_rand();

        // forwarded packets will not be forwarded again
        let forwarded_packet = {
            let transaction =
                system_transaction::transfer(&keypair, &pubkey, rent_min_balance, blockhash);
            let mut packet = Packet::from_data(None, transaction).unwrap();
            packet.meta_mut().flags |= PacketFlags::FORWARDED;
            DeserializedPacket::new(packet).unwrap()
        };
        // packets from unstaked nodes will not be forwarded
        let unstaked_packet = {
            let transaction =
                system_transaction::transfer(&keypair, &pubkey, rent_min_balance, blockhash);
            let packet = Packet::from_data(None, transaction).unwrap();
            DeserializedPacket::new(packet).unwrap()
        };
        // packets with incorrect blockhash will be filtered out
        let incorrect_blockhash_packet = {
            let transaction =
                system_transaction::transfer(&keypair, &pubkey, rent_min_balance, Hash::default());
            let packet = Packet::from_data(None, transaction).unwrap();
            DeserializedPacket::new(packet).unwrap()
        };

        // maybe also add packet without stake and packet with incorrect blockhash?
        let (expected_packet_size, normal_packet) = {
            let transaction = system_transaction::transfer(&keypair, &pubkey, 1, blockhash);
            let mut packet = Packet::from_data(None, transaction).unwrap();
            packet.meta_mut().set_from_staked_node(true);
            (packet.meta().size, DeserializedPacket::new(packet).unwrap())
        };

        let mut unprocessed_packet_batches = UnprocessedTransactionStorage::new_transaction_storage(
            UnprocessedPacketBatches::from_iter(
                vec![
                    forwarded_packet,
                    unstaked_packet,
                    incorrect_blockhash_packet,
                    normal_packet,
                ],
                4,
            ),
            ThreadType::Transactions,
        );

        let test_cases = vec![
            ("fwd-normal", true, 2, 1),
            ("fwd-no-op", true, 2, 0),
            ("fwd-no-hold", false, 0, 0),
        ];

        let leader_updater =
            ForwarderLeaderUpdater::new(cluster_info.clone(), poh_recorder.clone());
        let client = ForwarderConnectionCacheClient::new(
            Arc::new(ConnectionCache::new("connection_cache_test")),
            leader_updater,
        );
        let mut forwarder = Forwarder::new(client, bank_forks, Arc::new(DataBudget::default()));
        let runtime = rt("solQuicTestRt".to_string());
        for (name, hold, expected_num_unprocessed, expected_num_processed) in test_cases {
            let stats = BankingStageStats::default();
            forwarder.handle_forwarding(
                &mut unprocessed_packet_batches,
                hold,
                &mut LeaderSlotMetricsTracker::new(0),
                &stats,
                &mut TracerPacketStats::new(0),
            );

            let recv_socket = &local_node.sockets.tpu_forwards_quic[0];

            runtime.block_on(check_all_received(
                (*recv_socket).try_clone().unwrap(),
                expected_num_processed,
                expected_packet_size,
                &blockhash,
            ));

            let num_unprocessed_packets: usize = unprocessed_packet_batches.len();
            assert_eq!(num_unprocessed_packets, expected_num_unprocessed, "{name}");
        }

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }
}
