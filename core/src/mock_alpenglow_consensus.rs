//#[allow(unused_imports)]
#![allow(warnings)]
use {
    crate::consensus::Stake,
    bytemuck::{Pod, Zeroable},
    chrono::TimeDelta,
    crossbeam_channel::{bounded, Receiver, Sender},
    futures::future::err,
    solana_clock::Slot,
    solana_gossip::{
        cluster_info::ClusterInfo,
        epoch_specs::{self, EpochSpecs},
    },
    solana_packet::{Meta, Packet, PACKET_DATA_SIZE},
    solana_pubkey::{Pubkey, PUBKEY_BYTES},
    solana_signature::{Signature, SIGNATURE_BYTES},
    solana_signer::Signer,
    solana_streamer::{recvmmsg::recv_mmsg, sendmmsg::batch_send},
    solana_turbine::cluster_nodes,
    static_assertions::const_assert,
    std::{
        collections::HashMap,
        io::Error,
        net::{SocketAddrV4, UdpSocket},
        sync::{atomic::AtomicBool, Arc, Mutex},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    },
};

/// Cast mock votes on slots that divide by 53 to avoid crashing the network
const MOCK_VOTE_INTERVAL: Slot = 53;

// This needs to be > 3 for current logic of metrics collection
const_assert!(MOCK_VOTE_INTERVAL > 3);

struct Counters {
    stake: Stake,
    vote_relative_time: Duration,
}

/// This holds the state for sender and listener threads
/// of the mock alpenglow behind a mutex. Contention on this
/// should be low since there is only 2 threads and one of them
/// only ever does anything exactly once per slot for ~1ms
struct SharedState {
    current_slot_start: Instant,
    peers: HashMap<Pubkey, Counters>,
    should_exit: bool,
    current_slot: Slot,
}

/// This is a placeholder that is only used for load-testing.
/// This is not representative of the actual alpenglow implementation.
pub(crate) struct MockAlpenglowConsensus {
    sender_thread: JoinHandle<()>,
    listener_thread: JoinHandle<()>,
    state: Arc<Mutex<SharedState>>,
    send_event: Sender<Slot>,
    epoch_specs: EpochSpecs,
}

/// Header of the mock vote packet.
/// Actual frames on the wire are `MOCK_VOTE_PACKET_BYTES` in length
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct MockVotePacketHeader {
    signature: [u8; SIGNATURE_BYTES],
    sender: [u8; PUBKEY_BYTES],
    slot_number: Slot,
}
const MOCK_VOTE_HEADER_SIZE: usize = std::mem::size_of::<MockVotePacketHeader>();
const MOCK_VOTE_PACKET_SIZE: usize = 1200;

impl MockVotePacketHeader {
    fn from_bytes_mut(buf: &mut [u8]) -> &mut Self {
        bytemuck::from_bytes_mut::<MockVotePacketHeader>(&mut buf[..MOCK_VOTE_HEADER_SIZE])
    }
    fn from_bytes(buf: &[u8]) -> &Self {
        bytemuck::from_bytes::<MockVotePacketHeader>(&buf[..MOCK_VOTE_HEADER_SIZE])
    }
}
const EXIT_CHECK_INTERVAL: Duration = Duration::from_millis(100);

impl MockAlpenglowConsensus {
    pub(crate) fn new(
        alpenglow_socket: UdpSocket,
        cluster_info: Arc<ClusterInfo>,
        epoch_specs: EpochSpecs,
    ) -> Self {
        //socket.set_nonblocking(true).unwrap();
        let socket = Arc::new(alpenglow_socket);
        let (vote_command_sender, vote_command_receiver) = bounded(4);
        let shared_state = Arc::new(Mutex::new(SharedState {
            should_exit: false,
            current_slot_start: Instant::now(),
            peers: HashMap::new(),
            current_slot: 0,
        }));
        Self {
            state: shared_state.clone(),
            listener_thread: thread::spawn({
                let shared_state = shared_state.clone();
                let socket = socket.clone();
                let cluster_info = cluster_info.clone();
                let epoch_specs = epoch_specs.clone();
                move || Self::listener_thread(shared_state, cluster_info, epoch_specs, socket)
            }),
            sender_thread: thread::spawn({
                let epoch_specs = epoch_specs.clone();
                move || {
                    Self::sender_thread(
                        shared_state,
                        cluster_info,
                        epoch_specs,
                        socket.clone(),
                        vote_command_receiver,
                    )
                }
            }),
            epoch_specs,
            send_event: vote_command_sender,
        }
    }

    /// Collects votes and prepares statistics
    fn listener_thread(
        state: Arc<Mutex<SharedState>>,
        cluster_info: Arc<ClusterInfo>,
        mut epoch_specs: EpochSpecs,
        socket: Arc<UdpSocket>,
    ) {
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        // Set aside enough space to fetch multiple packets from the kernel per syscall
        let mut packets: Vec<Packet> = vec![Packet::default(); 1024];
        loop {
            // must wipe all Meta records to reuse the buffer
            for p in packets.iter_mut() {
                *p.meta_mut() = Meta::default();
            }
            // recv_mmsg should timeout in 1 second
            let n = match recv_mmsg(&socket, &mut packets) {
                Ok(n) => n,
                Err(e) => {
                    match e.kind() {
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                            0 // no packets received
                        }
                        _ => {
                            error!("Got error {:?} in mock alpenglow RX socket operation, exiting thread", e.raw_os_error());
                            return;
                        }
                    }
                }
            };
            let mut lockguard = state.lock().unwrap();
            if lockguard.should_exit {
                return;
            }
            // we may have received no packets, in this case we can safely skip the rest
            if n == 0 {
                continue;
            }
            let elapsed = lockguard.current_slot_start.elapsed();
            for pkt in packets.iter().take(n) {
                if pkt.meta().size < MOCK_VOTE_HEADER_SIZE {
                    continue;
                }
                let Some(pkt_buf) = pkt.data(..) else {
                    continue;
                };
                let vote_pkt = MockVotePacketHeader::from_bytes(pkt_buf);
                let pk = Pubkey::new_from_array(vote_pkt.sender);
                if vote_pkt.slot_number != lockguard.current_slot {
                    continue;
                }
                let Some(&stake) = epoch_specs.current_epoch_staked_nodes().get(&pk) else {
                    continue;
                };
                let signature = Signature::from(vote_pkt.signature);
                trace!("Got vote for slot {} from {}", vote_pkt.slot_number, pk);
                if signature.verify(pk.as_array(), &pkt_buf[SIGNATURE_BYTES..]) {
                    lockguard.peers.insert(
                        pk,
                        Counters {
                            stake,
                            vote_relative_time: elapsed,
                        },
                    );
                }
            }
        }
    }

    /// Sends fake votes to everyone in the cluster
    /// Also resets counters for the next slot
    fn sender_thread(
        state: Arc<Mutex<SharedState>>,
        cluster_info: Arc<ClusterInfo>,
        mut epoch_specs: EpochSpecs,
        socket: Arc<UdpSocket>,
        new_slot: Receiver<Slot>,
    ) {
        let mut packet_buf = vec![0u8; MOCK_VOTE_PACKET_SIZE];
        let id = cluster_info.id();

        loop {
            let Ok(slot) = new_slot.recv_timeout(EXIT_CHECK_INTERVAL) else {
                if state.lock().unwrap().should_exit {
                    return;
                }
                continue;
            };

            // prepare the packet to send and sign it
            {
                let pkt = MockVotePacketHeader::from_bytes_mut(&mut packet_buf);
                pkt.slot_number = slot;
                pkt.sender = *id.as_array();
                pkt.signature = [0; SIGNATURE_BYTES];
            }
            let signature = cluster_info
                .keypair()
                .sign_message(&packet_buf[SIGNATURE_BYTES..]);
            {
                let pkt = MockVotePacketHeader::from_bytes_mut(&mut packet_buf);
                pkt.signature = *signature.as_array();
            }

            // prepare addresses to send the packets
            let mut send_instructions = Vec::with_capacity(2000);
            for peer in epoch_specs.current_epoch_staked_nodes().keys() {
                let Some(ag_addr) = cluster_info
                    .lookup_contact_info(peer, |ci| ci.alpenglow())
                    .flatten()
                else {
                    continue;
                };
                send_instructions.push((packet_buf.as_slice(), ag_addr));
                trace!("Sending mock vote for slot {slot} to {ag_addr} for {peer}");
            }

            // broadcast to everybody at once
            batch_send(&socket, send_instructions);
        }
    }

    fn report_collected_votes(peers: &HashMap<Pubkey, Counters>, total_staked: Stake) {
        let mut total_voted_stake: Stake = 0;
        let mut total_voted_nodes: usize = 0;
        let mut total_delay_ms = 0u128;
        for (pubkey, counters) in peers.iter() {
            total_voted_stake += counters.stake;
            total_voted_nodes += 1;
            total_delay_ms += (counters.vote_relative_time.as_millis().clamp(0, 400) as u128)
                * counters.stake as u128;
        }

        let stake_weighted_delay = if total_voted_stake > 0 {
            total_delay_ms as f64 / total_voted_stake as f64
        } else {
            0.0
        };
        let percent_collected = 100.0 * total_voted_stake as f64 / total_staked as f64;
        info!(
            "Got {} % of total stake collected, stake-weighted delay is {}ms",
            percent_collected, stake_weighted_delay
        );
        datapoint_info!(
            "mock_alpenglow",
            ("total_peers", peers.len(), f64),
            ("packets_collected", total_voted_nodes, f64),
            ("percent_stake_collected", percent_collected, f64),
            ("weighted_delay_ms", stake_weighted_delay, f64),
        );
    }

    pub(crate) fn signal_new_slot(&mut self, slot: Slot) {
        let phase = slot % MOCK_VOTE_INTERVAL;
        // this is currently set up to work over a course of 3 slots.
        // it will only work correctly if we do not actually fork within those 3 slots, and
        // are consistently casting votes for all 3.
        match phase {
            // Clear the stats for the previous slots in preparation for the new one
            0 => {
                let mut lockguard = self.state.lock().unwrap();
                lockguard.peers.clear();
                // wait for votes for the next slot in case we are really late getting shreds
                lockguard.current_slot = slot + 1;
                lockguard.current_slot_start = Instant::now();
            }
            // send votes
            1 => {
                self.send_event.send(slot);
            }
            // report metrics
            2 => {
                let mut lockguard = self.state.lock().unwrap();
                let total_staked: Stake =
                    self.epoch_specs.current_epoch_staked_nodes().values().sum();
                Self::report_collected_votes(&lockguard.peers, total_staked);
                lockguard.peers.clear();
                lockguard.current_slot = 0; // block reception of votes
            }
            // in case we have missed previous slot, clean up and block reception
            // but do not report stats (as they will be all garbled by now)
            _ => {
                let mut lockguard = self.state.lock().unwrap();
                lockguard.current_slot = 0; // block reception of votes
                lockguard.peers.clear();
            }
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.state.lock().unwrap().should_exit = true;
        self.sender_thread.join()?;
        self.listener_thread.join()
    }
}
