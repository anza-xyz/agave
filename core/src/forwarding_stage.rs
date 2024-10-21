use {
    crate::{
        banking_stage::LikeClusterInfo,
        banking_trace::BankingPacketBatch,
        next_leader::{next_leader, next_leader_tpu_vote},
    },
    crossbeam_channel::Receiver,
    solana_client::connection_cache::ConnectionCache,
    solana_connection_cache::client_connection::ClientConnection,
    solana_gossip::contact_info::Protocol,
    solana_perf::data_budget::DataBudget,
    solana_poh::poh_recorder::PohRecorder,
    solana_sdk::timing::AtomicInterval,
    solana_streamer::sendmmsg::batch_send,
    std::{
        iter::repeat,
        net::{SocketAddr, UdpSocket},
        sync::{Arc, RwLock},
        thread::{Builder, JoinHandle},
    },
};

pub trait ForwardAddressGetter: Send + Sync + 'static {
    fn get_forwarding_address(&self, tpu_vote: bool, protocol: Protocol) -> Option<SocketAddr>;
}

impl<T: LikeClusterInfo> ForwardAddressGetter for (T, Arc<RwLock<PohRecorder>>) {
    fn get_forwarding_address(&self, tpu_vote: bool, protocol: Protocol) -> Option<SocketAddr> {
        if tpu_vote {
            next_leader_tpu_vote(&self.0, &self.1)
        } else {
            next_leader(&self.0, &self.1, |node| node.tpu_forwards(protocol))
        }
        .map(|(_, addr)| addr)
    }
}

pub struct ForwardingStage<T: ForwardAddressGetter> {
    receiver: Receiver<(BankingPacketBatch, bool)>,
    forward_address_getter: T,
    connection_cache: Arc<ConnectionCache>,
    data_budget: DataBudget,
    udp_socket: UdpSocket,
    metrics: ForwardingStageMetrics,
}

impl<T: ForwardAddressGetter> ForwardingStage<T> {
    pub fn spawn(
        receiver: Receiver<(BankingPacketBatch, bool)>,
        forward_address_getter: T,
        connection_cache: Arc<ConnectionCache>,
    ) -> JoinHandle<()> {
        let forwarding_stage = Self::new(receiver, forward_address_getter, connection_cache);
        Builder::new()
            .name("solFwdStage".to_string())
            .spawn(move || forwarding_stage.run())
            .unwrap()
    }

    fn new(
        receiver: Receiver<(BankingPacketBatch, bool)>,
        forward_address_getter: T,
        connection_cache: Arc<ConnectionCache>,
    ) -> Self {
        Self {
            receiver,
            forward_address_getter,
            connection_cache,
            data_budget: DataBudget::default(),
            udp_socket: UdpSocket::bind("0.0.0.0:0").unwrap(),
            metrics: ForwardingStageMetrics::default(),
        }
    }

    fn run(mut self) {
        while let Ok((packet_batches, tpu_vote_batch)) = self.receiver.recv() {
            self.metrics.maybe_report();
            self.handle_batches(packet_batches, tpu_vote_batch);
        }
    }

    fn handle_batches(&mut self, packet_batches: BankingPacketBatch, tpu_vote_batch: bool) {
        // Count the number of packets received
        let num_packets_received: usize = packet_batches.0.iter().map(|batch| batch.len()).sum();
        if tpu_vote_batch {
            self.metrics.vote_packets_received += num_packets_received;
        } else {
            self.metrics.non_vote_packets_received += num_packets_received;
        }

        let Some(addr) = self
            .forward_address_getter
            .get_forwarding_address(tpu_vote_batch, self.connection_cache.protocol())
        else {
            return;
        };

        self.update_data_budget();
        self.forward_batches(packet_batches, tpu_vote_batch, addr);
    }

    fn forward_batches(
        &mut self,
        packet_batches: BankingPacketBatch,
        tpu_vote_batch: bool,
        addr: SocketAddr,
    ) {
        let filtered_packets = packet_batches
            .0
            .iter()
            .flat_map(|batch| batch.iter())
            .filter(|p| !p.meta().discard())
            .filter(|p| !p.meta().forwarded())
            .filter(|p| p.meta().is_from_staked_node())
            .filter(|p| self.data_budget.take(p.meta().size))
            .filter_map(|p| p.data(..).map(|data| data.to_vec()));

        let (success, num_packets) = if tpu_vote_batch {
            let packets: Vec<_> = filtered_packets.into_iter().zip(repeat(addr)).collect();
            let num_packets = packets.len();
            let res = batch_send(&self.udp_socket, &packets);
            (res.is_ok(), num_packets)
        } else {
            let packets: Vec<_> = filtered_packets.collect();
            let num_packets = packets.len();
            let conn = self.connection_cache.get_connection(&addr);
            let res = conn.send_data_batch_async(packets);
            (res.is_ok(), num_packets)
        };

        if tpu_vote_batch {
            self.metrics.vote_packets_attempted_forward += num_packets;
            if success {
                self.metrics.vote_packets_forwarded += num_packets;
            }
        } else {
            self.metrics.non_vote_packets_attempted_forward += num_packets;
            if success {
                self.metrics.non_vote_packets_forwarded += num_packets;
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
}

#[derive(Default)]
struct ForwardingStageMetrics {
    interval: AtomicInterval,

    vote_packets_received: usize,
    vote_packets_attempted_forward: usize,
    vote_packets_forwarded: usize,

    non_vote_packets_received: usize,
    non_vote_packets_attempted_forward: usize,
    non_vote_packets_forwarded: usize,
}

impl ForwardingStageMetrics {
    fn maybe_report(&mut self) {
        const REPORT_INTERVAL_MS: u64 = 5000;
        if self.interval.should_update(REPORT_INTERVAL_MS) {
            datapoint_info!(
                "forwarding_stage",
                (
                    "vote_packets_received",
                    core::mem::replace(&mut self.vote_packets_received, 0),
                    i64
                ),
                (
                    "vote_packets_attempted_forward",
                    core::mem::replace(&mut self.vote_packets_attempted_forward, 0),
                    i64
                ),
                (
                    "vote_packets_forwarded",
                    core::mem::replace(&mut self.vote_packets_forwarded, 0),
                    i64
                ),
                (
                    "non_vote_packets_received",
                    core::mem::replace(&mut self.non_vote_packets_received, 0),
                    i64
                ),
                (
                    "non_vote_packets_attempted_forward",
                    core::mem::replace(&mut self.non_vote_packets_attempted_forward, 0),
                    i64
                ),
                (
                    "non_vote_packets_forwarded",
                    core::mem::replace(&mut self.non_vote_packets_forwarded, 0),
                    i64
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_perf::packet::PacketBatch,
        solana_sdk::packet::{Packet, PacketFlags},
        std::time::Duration,
    };

    struct UnimplementedForwardAddressGetter;
    impl ForwardAddressGetter for UnimplementedForwardAddressGetter {
        fn get_forwarding_address(
            &self,
            _tpu_vote: bool,
            _protocol: Protocol,
        ) -> Option<SocketAddr> {
            unimplemented!("UnimplementedForwardAddressGetter:get_forwarding_address")
        }
    }

    struct DummyForwardAddressGetter {
        vote_addr: SocketAddr,
        non_vote_addr: SocketAddr,
    }
    impl ForwardAddressGetter for DummyForwardAddressGetter {
        fn get_forwarding_address(
            &self,
            tpu_vote: bool,
            _protocol: Protocol,
        ) -> Option<SocketAddr> {
            Some(if tpu_vote {
                self.vote_addr
            } else {
                self.non_vote_addr
            })
        }
    }

    // Create simple dummy packets with different flags
    // to filter out packets that should not be forwarded.
    fn create_dummy_packet(num_bytes: usize, flags: PacketFlags) -> Packet {
        let mut packet = Packet::default();
        packet.meta_mut().size = num_bytes;
        packet.meta_mut().flags = flags;
        packet
    }

    #[test]
    fn test_forward_batches() {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let addr = socket.local_addr().unwrap();

        let (_sender, receiver) = crossbeam_channel::unbounded();
        let dummy_forward_address_getter = UnimplementedForwardAddressGetter;
        let connection_cache = ConnectionCache::with_udp("connection_cache_test", 1);

        let mut forwarding_stage = ForwardingStage::new(
            receiver,
            dummy_forward_address_getter,
            Arc::new(connection_cache),
        );

        const DUMMY_PACKET_SIZE: usize = 8;
        let create_banking_packet_batch = |flags| {
            BankingPacketBatch::new((
                vec![PacketBatch::new(vec![create_dummy_packet(
                    DUMMY_PACKET_SIZE,
                    flags,
                )])],
                None,
            ))
        };

        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let recv_buffer = &mut [0; 1024];

        // Data Budget is 0, so no packets should be sent
        for tpu_vote in [false, true] {
            forwarding_stage.forward_batches(
                create_banking_packet_batch(PacketFlags::FROM_STAKED_NODE),
                tpu_vote,
                addr,
            );
            assert!(socket.recv_from(recv_buffer).is_err());
        }

        for tpu_vote in [false, true] {
            // Packet is valid, so it should be sent
            forwarding_stage.update_data_budget();
            forwarding_stage.forward_batches(
                create_banking_packet_batch(PacketFlags::FROM_STAKED_NODE),
                tpu_vote,
                addr,
            );
            assert_eq!(socket.recv_from(recv_buffer).unwrap().0, DUMMY_PACKET_SIZE,);

            // Packet is marked for discard, so it should not be sent
            forwarding_stage.update_data_budget();
            forwarding_stage.forward_batches(
                create_banking_packet_batch(PacketFlags::FROM_STAKED_NODE | PacketFlags::DISCARD),
                tpu_vote,
                addr,
            );
            assert!(socket.recv_from(recv_buffer).is_err());

            // Packet is not from staked node, so it should not be sent
            forwarding_stage.update_data_budget();
            forwarding_stage.forward_batches(
                create_banking_packet_batch(PacketFlags::empty()),
                tpu_vote,
                addr,
            );
            assert!(socket.recv_from(recv_buffer).is_err());

            // Packet is already forwarded, so it should not be sent
            forwarding_stage.update_data_budget();
            forwarding_stage.forward_batches(
                create_banking_packet_batch(PacketFlags::FROM_STAKED_NODE | PacketFlags::FORWARDED),
                tpu_vote,
                addr,
            );
            assert!(socket.recv_from(recv_buffer).is_err());
        }
    }

    #[test]
    fn test_handle_batches() {
        let vote_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let non_vote_socket = UdpSocket::bind("0.0.0.0:0").unwrap();

        let (_sender, receiver) = crossbeam_channel::unbounded();
        let dummy_forward_address_getter = DummyForwardAddressGetter {
            vote_addr: vote_socket.local_addr().unwrap(),
            non_vote_addr: non_vote_socket.local_addr().unwrap(),
        };
        let connection_cache = ConnectionCache::with_udp("connection_cache_test", 1);

        let mut forwarding_stage = ForwardingStage::new(
            receiver,
            dummy_forward_address_getter,
            Arc::new(connection_cache),
        );

        // packet sizes should be unique and paired with
        // `expected_received` below so we can verify the correct packets
        // were forwarded.
        let banking_packet_batches = BankingPacketBatch::new((
            vec![
                PacketBatch::new(vec![
                    create_dummy_packet(1, PacketFlags::empty()), // invalid
                    create_dummy_packet(2, PacketFlags::FROM_STAKED_NODE), // valid
                    create_dummy_packet(3, PacketFlags::DISCARD), // invalid
                    create_dummy_packet(4, PacketFlags::FORWARDED), // invalid
                ]),
                PacketBatch::new(vec![
                    create_dummy_packet(5, PacketFlags::FROM_STAKED_NODE), // valid
                    create_dummy_packet(6, PacketFlags::DISCARD),          // invalid
                    create_dummy_packet(7, PacketFlags::FROM_STAKED_NODE), // valid
                    create_dummy_packet(8, PacketFlags::FORWARDED),        // invalid
                    create_dummy_packet(9, PacketFlags::FROM_STAKED_NODE | PacketFlags::DISCARD), // invalid
                ]),
            ],
            None,
        ));
        let expected_received = [2, 5, 7];

        let recv_buffer = &mut [0; 1024];
        for (is_tpu_vote, socket) in [(true, vote_socket), (false, non_vote_socket)] {
            socket
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();

            forwarding_stage.handle_batches(banking_packet_batches.clone(), is_tpu_vote);

            for expected_size in &expected_received {
                assert_eq!(socket.recv_from(recv_buffer).unwrap().0, *expected_size);
            }
            assert!(socket.recv_from(recv_buffer).is_err()); // nothing more to receive
        }
    }
}
