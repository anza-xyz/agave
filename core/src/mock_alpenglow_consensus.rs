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
    solana_keypair::Keypair,
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
        net::{SocketAddr, SocketAddrV4, UdpSocket},
        sync::{atomic::AtomicBool, Arc, Mutex},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    },
};

/// Cast mock votes on slots that divide by 53 to avoid crashing the network
const MOCK_VOTE_INTERVAL: Slot = 53;

// This needs to be > 3 for current logic of metrics collection
const_assert!(MOCK_VOTE_INTERVAL > 3);

struct PeerData {
    stake: Stake,
    address: SocketAddr,
    relative_toa: [Option<Duration>; 4],
}

/// This holds the state for sender and listener threads
/// of the mock alpenglow behind a mutex. Contention on this
/// should be low since there is only 2 threads and one of them
/// only ever does anything exactly once per slot for ~1ms
struct SharedState {
    current_slot_start: Instant,
    peers: HashMap<Pubkey, PeerData>,
    should_exit: bool,
    current_slot: Slot,
}

/// This is a placeholder that is only used for load-testing.
/// This is not representative of the actual alpenglow implementation.
pub(crate) struct MockAlpenglowConsensus {
    sender_thread: JoinHandle<()>,
    listener_thread: JoinHandle<()>,
    state: Arc<Mutex<SharedState>>,
    command_sender: Sender<Command>,
    epoch_specs: EpochSpecs,
}

#[derive(Copy, Clone, Debug)]
#[repr(u64)]
enum AlpenglowState {
    Invalid,
    Notarize,
    NotarizeCertificate,
    Finalize,
    FinalizeCertificate,
}

impl TryFrom<u64> for AlpenglowState {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Notarize),
            2 => Ok(Self::NotarizeCertificate),
            3 => Ok(Self::Finalize),
            4 => Ok(Self::FinalizeCertificate),
            _ => Err(()),
        }
    }
}
/// Header of the mock vote packet.
/// Actual frames on the wire are `MOCK_VOTE_PACKET_BYTES` in length
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct MockVotePacketHeader {
    signature: [u8; SIGNATURE_BYTES],
    sender: [u8; PUBKEY_BYTES],
    slot_number: Slot,
    state: u64,
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

#[derive(Debug, Clone, Copy)]
pub(crate) enum Command {
    SendNotarize(Slot),
    SendNotarizeCertificate(Slot),
    SendFinalize(Slot),
    SendFinalizeCertificate(Slot),
}

impl MockAlpenglowConsensus {
    pub(crate) fn new(
        alpenglow_socket: UdpSocket,
        cluster_info: Arc<ClusterInfo>,
        epoch_specs: EpochSpecs,
    ) -> Self {
        //socket.set_nonblocking(true).unwrap();
        let socket = Arc::new(alpenglow_socket);
        let (command_sender, vote_command_receiver) = bounded(4);
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
            command_sender,
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
                let Ok(state) = AlpenglowState::try_from(vote_pkt.state) else {
                    continue;
                };
                let Some(peer_info) = lockguard.peers.get_mut(&pk) else {
                    continue;
                };
                let signature = Signature::from(vote_pkt.signature);
                trace!("RX slot {}: {:?} from {}", vote_pkt.slot_number, state, pk);
                if signature.verify(pk.as_array(), &pkt_buf[SIGNATURE_BYTES..]) {
                    peer_info.relative_toa[state as u64 as usize] = Some(elapsed);
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
        command: Receiver<Command>,
    ) {
        let mut packet_buf = vec![0u8; MOCK_VOTE_PACKET_SIZE];
        let id = cluster_info.id();

        loop {
            let Ok(command) = command.recv_timeout(EXIT_CHECK_INTERVAL) else {
                if state.lock().unwrap().should_exit {
                    return;
                }
                continue;
            };
            let (slot, ag_state) = match command {
                Command::SendNotarize(slot) => (slot, AlpenglowState::Notarize),
                Command::SendNotarizeCertificate(slot) => {
                    (slot, AlpenglowState::NotarizeCertificate)
                }
                Command::SendFinalize(slot) => (slot, AlpenglowState::Finalize),
                Command::SendFinalizeCertificate(slot) => {
                    (slot, AlpenglowState::FinalizeCertificate)
                }
            };
            prep_and_sign_packet(
                &mut packet_buf,
                slot,
                &id,
                ag_state,
                cluster_info.keypair().as_ref(),
            );

            // prepare addresses to send the packets
            let send_instructions = match ag_state {
                AlpenglowState::Notarize => {
                    let staked_nodes = epoch_specs.current_epoch_staked_nodes();
                    let mut lockguard = state.lock().unwrap();
                    let mut send_instructions = Vec::with_capacity(2000);
                    for (peer, &stake) in staked_nodes.iter() {
                        let Some(ag_addr) = cluster_info
                            .lookup_contact_info(peer, |ci| ci.alpenglow())
                            .flatten()
                        else {
                            continue;
                        };
                        send_instructions.push((packet_buf.as_slice(), ag_addr));
                        lockguard.peers.insert(
                            *peer,
                            PeerData {
                                stake: stake,
                                address: ag_addr,
                                relative_toa: [None; 4],
                            },
                        );
                        trace!("Sending mock vote for slot {slot} to {ag_addr} for {peer}");
                    }
                    send_instructions
                }
                _ => {
                    let mut send_instructions = Vec::with_capacity(2000);
                    let mut lockguard = state.lock().unwrap();

                    for (peer, info) in lockguard.peers.iter() {
                        send_instructions.push((packet_buf.as_slice(), info.address));
                    }
                    send_instructions
                }
            };

            // broadcast to everybody at once
            batch_send(&socket, send_instructions);
        }
    }

    fn report_collected_votes(peers: &HashMap<Pubkey, PeerData>, total_staked: Stake) {
        let mut total_voted_stake: [Stake; 4] = [0; 4];
        let mut total_voted_nodes: [usize; 4] = [0; 4];
        let mut total_delay_ms = [0u128; 4];
        for (pubkey, counters) in peers.iter() {
            for i in 0..4 {
                let Some(rel_toa) = counters.relative_toa[i] else {
                    continue;
                };
                total_voted_stake[i] += counters.stake;
                total_voted_nodes[i] += 1;
                total_delay_ms[i] +=
                    (rel_toa.as_millis().clamp(0, 800) as u128) * counters.stake as u128;
            }
        }

        let mut stake_weighted_delay = [0f64; 4];
        let mut percent_collected = [0f64; 4];

        for i in 0..4 {
            if total_voted_stake[i] > 0 {
                stake_weighted_delay[i] = total_delay_ms[i] as f64 / total_voted_stake[i] as f64;
            }
            percent_collected[i] = 100.0 * total_voted_stake[i] as f64 / total_staked as f64;

            info!(
                "{:?}: got {} % of total stake collected, stake-weighted delay is {}ms",
                AlpenglowState::try_from(i as u64).unwrap_or(AlpenglowState::Invalid),
                percent_collected[i],
                stake_weighted_delay[i]
            );
        }
        datapoint_info!(
            "mock_alpenglow",
            ("total_peers", peers.len(), f64),
            ("packets_collected_notarize", total_voted_nodes[0], f64),
            (
                "percent_stake_collected_notarize",
                percent_collected[0],
                f64
            ),
            ("weighted_delay_ms_notarize", stake_weighted_delay[0], f64),
            ("packets_collected_notarize_cert", total_voted_nodes[1], f64),
            (
                "percent_stake_collected_notarize_cert",
                percent_collected[1],
                f64
            ),
            (
                "weighted_delay_ms_notarize_cert",
                stake_weighted_delay[1],
                f64
            ),
            ("packets_collected_finalize", total_voted_nodes[2], f64),
            (
                "percent_stake_collected_finalize",
                percent_collected[2],
                f64
            ),
            ("weighted_delay_ms_finalize", stake_weighted_delay[2], f64),
            ("packets_collected_finalize_cert", total_voted_nodes[3], f64),
            (
                "percent_stake_collected_finalize_cert",
                percent_collected[3],
                f64
            ),
            (
                "weighted_delay_ms_finalize_cert",
                stake_weighted_delay[3],
                f64
            ),
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
            // send Notarize votes (rest of the consensus happens based on received packets)
            1 => {
                self.command_sender.send(Command::SendNotarize(slot));
            }
            // report metrics
            2 => {
                let mut lockguard = self.state.lock().unwrap();
                let total_staked: Stake = lockguard.peers.values().map(|v| v.stake).sum();
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

fn prep_and_sign_packet(
    packet_buf: &mut [u8],
    slot: Slot,
    id: &Pubkey,
    state: AlpenglowState,
    keypair: &Keypair,
) {
    // prepare the packet to send and sign it
    {
        let pkt = MockVotePacketHeader::from_bytes_mut(packet_buf);
        pkt.slot_number = slot;
        pkt.sender = *id.as_array();
        pkt.signature = [0; SIGNATURE_BYTES];
        pkt.state = state as u64;
    }
    let signature = keypair.sign_message(&packet_buf[SIGNATURE_BYTES..]);
    {
        let pkt = MockVotePacketHeader::from_bytes_mut(packet_buf);
        pkt.signature = *signature.as_array();
    }
}
