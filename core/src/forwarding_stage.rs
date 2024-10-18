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
    std::{
        iter::repeat,
        net::{SocketAddr, UdpSocket},
        sync::{Arc, RwLock},
        thread::{Builder, JoinHandle},
    },
};

pub struct ForwardingStage<T: LikeClusterInfo> {
    receiver: BankingPacketReceiver,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    cluster_info: T,
    connection_cache: Arc<ConnectionCache>,
    data_budget: DataBudget,
    udp_socket: UdpSocket,
}

impl<T: LikeClusterInfo> ForwardingStage<T> {
    pub fn spawn(
        receiver: BankingPacketReceiver,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        cluster_info: T,
        connection_cache: Arc<ConnectionCache>,
    ) -> JoinHandle<()> {
        let forwarding_stage = Self {
            receiver,
            poh_recorder,
            cluster_info,
            connection_cache,
            data_budget: DataBudget::default(),
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
            let Some((_leader, leader_address)) = self.get_leader_and_addr(tpu_vote_batch) else {
                // If unknown leader, move to next packet batch.
                continue;
            };

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
                let pkts: Vec<_> = packet_vec.into_iter().zip(repeat(leader_address)).collect();
                let _ = batch_send(&self.udp_socket, &pkts);
            } else {
                let conn = self.connection_cache.get_connection(&leader_address);
                let _ = conn.send_data_batch_async(packet_vec);
            }
        }
    }

    /// Get the pubkey and socket address for the leader to forward to
    fn get_leader_and_addr(&self, tpu_vote: bool) -> Option<(Pubkey, SocketAddr)> {
        if tpu_vote {
            next_leader_tpu_vote(&self.cluster_info, &self.poh_recorder)
        } else {
            next_leader(&self.cluster_info, &self.poh_recorder, |node| {
                node.tpu_forwards(self.connection_cache.protocol())
            })
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
