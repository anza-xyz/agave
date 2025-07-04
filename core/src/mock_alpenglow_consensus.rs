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

struct PeerData {
    stake: Stake,
    address: SocketAddr,
    relative_toa: [Option<Duration>; 4],
}

#[derive(Default, Debug)]
struct AgStateMachine {
    block_notarized: bool,
    block_has_notar_cert: bool,
    block_finalized: bool,
    notarize_stake_collected: Stake,
    finalize_stake_collected: Stake,
}

/// This holds the state for sender and listener threads
/// of the mock alpenglow behind a mutex. Contention on this
/// should be low since there is only 2 threads and one of them
/// only ever does anything exactly once per slot for ~1ms
struct SharedState {
    current_slot_start: Instant,
    peers: HashMap<Pubkey, PeerData>,
    total_staked: Stake,
    should_exit: bool,
    current_slot: Slot,
    alpenglow_state: AgStateMachine,
}

impl SharedState {
    fn reset(&mut self) -> HashMap<Pubkey, PeerData> {
        let mut peers = HashMap::new();
        std::mem::swap(&mut peers, &mut self.peers);
        self.current_slot = 0;
        self.total_staked = 0;
        self.alpenglow_state = AgStateMachine::default();
        peers
    }
}

/// This is a placeholder that is only used for load-testing.
/// This is not representative of the actual alpenglow implementation.
pub(crate) struct MockAlpenglowConsensus {
    cluster_info: Arc<ClusterInfo>,
    sender_thread: JoinHandle<()>,
    listener_thread: JoinHandle<()>,
    state: Arc<Mutex<SharedState>>,
    command_sender: Sender<Command>,
    epoch_specs: EpochSpecs,
}

#[derive(Copy, Clone, Debug)]
#[repr(u64)]
enum VotorMessageType {
    Notarize,
    NotarizeCertificate,
    Finalize,
    // In this mockup, this acts as both Finalize and FastFinalize certificate
    FinalizeCertificate,
}

impl TryFrom<u64> for VotorMessageType {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Notarize),
            1 => Ok(Self::NotarizeCertificate),
            2 => Ok(Self::Finalize),
            3 => Ok(Self::FinalizeCertificate),
            _ => Err(()),
        }
    }
}
/// Header of the mock vote packet.
/// Actual frames on the wire are `MOCK_VOTE_PACKET_SIZE` in length
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct MockVotePacketHeader {
    signature: [u8; SIGNATURE_BYTES],
    sender: [u8; PUBKEY_BYTES],
    slot_number: Slot,
    state: u64,
}

const MOCK_VOTE_HEADER_SIZE: usize = std::mem::size_of::<MockVotePacketHeader>();

/// The actual alpenglow votor packets are all smaller than this,
/// but this is deliberately overtuned to model the worst case.
const MOCK_VOTE_PACKET_SIZE: usize = 512;

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
        info!("Mock Alpenglow consensus is enabled");
        let socket = Arc::new(alpenglow_socket);
        let (command_sender, vote_command_receiver) = bounded(4);
        let shared_state = Arc::new(Mutex::new(SharedState {
            should_exit: false,
            current_slot_start: Instant::now(),
            peers: HashMap::new(),
            current_slot: 0,
            total_staked: 0,
            alpenglow_state: AgStateMachine::default(),
        }));
        Self {
            state: shared_state.clone(),
            listener_thread: thread::spawn({
                let shared_state = shared_state.clone();
                let socket = socket.clone();
                let cluster_info = cluster_info.clone();
                let epoch_specs = epoch_specs.clone();
                let command_sender = command_sender.clone();
                move || {
                    Self::listener_thread(
                        shared_state,
                        cluster_info,
                        epoch_specs,
                        socket,
                        command_sender,
                    )
                }
            }),
            sender_thread: thread::spawn({
                let epoch_specs = epoch_specs.clone();
                let cluster_info = cluster_info.clone();
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
            cluster_info,
        }
    }

    fn prepare_to_receive(&mut self, slot: Slot) {
        let staked_nodes = self.epoch_specs.current_epoch_staked_nodes();
        let mut state = self.state.lock().unwrap();
        state.reset();
        state.current_slot = slot;
        state.current_slot_start = Instant::now();
        for (peer, &stake) in staked_nodes.iter() {
            let Some(ag_addr) = self
                .cluster_info
                .lookup_contact_info(peer, |ci| ci.alpenglow())
                .flatten()
            else {
                continue;
            };
            state.peers.insert(
                *peer,
                PeerData {
                    stake: stake,
                    address: ag_addr,
                    relative_toa: [None; 4],
                },
            );
            state.total_staked += stake;
        }
        trace!(
            "Prepared for slot {slot}, total stake is {}",
            state.total_staked
        );
    }

    /// Collects votes and changes states
    fn listener_thread(
        self_state: Arc<Mutex<SharedState>>,
        cluster_info: Arc<ClusterInfo>,
        mut epoch_specs: EpochSpecs,
        socket: Arc<UdpSocket>,
        command_sender: Sender<Command>,
    ) {
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let id = cluster_info.id();
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
            let mut state = self_state.lock().unwrap();
            if state.should_exit {
                return;
            }
            // we may have received no packets, in this case we can safely skip the rest
            if n == 0 {
                continue;
            }
            let elapsed = state.current_slot_start.elapsed();

            let stake_60_percent = (state.total_staked as f64 * 0.6) as Stake;
            let stake_80_percent = (state.total_staked as f64 * 0.8) as Stake;
            for pkt in packets.iter().take(n) {
                if pkt.meta().size < MOCK_VOTE_HEADER_SIZE {
                    continue;
                }
                let Some(pkt_buf) = pkt.data(..) else {
                    continue;
                };
                let vote_pkt = MockVotePacketHeader::from_bytes(pkt_buf);
                let pk = Pubkey::new_from_array(vote_pkt.sender);
                if vote_pkt.slot_number != state.current_slot {
                    continue;
                }
                let Ok(votor_msg) = VotorMessageType::try_from(vote_pkt.state) else {
                    continue;
                };
                let Some(peer_info) = state.peers.get_mut(&pk) else {
                    continue;
                };
                let signature = Signature::from(vote_pkt.signature);
                trace!(
                    "RX slot {}: {:?} from {}",
                    vote_pkt.slot_number,
                    votor_msg,
                    pk
                );
                let toa = &mut peer_info.relative_toa[votor_msg as u64 as usize];
                if toa.is_none() {
                    *toa = Some(elapsed);
                } else {
                    // duplicate packet received, ignore it
                    trace!("Duplicate packet");
                    continue;
                }
                // keep borrow checker happy
                let stake = peer_info.stake;
                match votor_msg {
                    VotorMessageType::Notarize => {
                        /*let c = state.alpenglow_state.notarize_stake_collected;
                        trace!("{id}:{c} + {}", peer_info.stake);
                        trace!(
                            "{id}:{} of {}",
                            state.alpenglow_state.notarize_stake_collected,
                            stake_60_percent
                        );*/
                        if !state.alpenglow_state.block_finalized {
                            if !signature.verify(pk.as_array(), &pkt_buf[SIGNATURE_BYTES..]) {
                                trace!("Sigverify failed");
                                continue;
                            }
                        }
                        if !state.alpenglow_state.block_notarized {
                            state.alpenglow_state.notarize_stake_collected += stake;
                            trace!(
                                "{id}:{} of {}",
                                state.alpenglow_state.notarize_stake_collected,
                                stake_60_percent
                            );
                            if state.alpenglow_state.notarize_stake_collected > stake_60_percent {
                                state.alpenglow_state.block_notarized = true;
                                trace!(
                                    "{id} has notarized slot {} by observing 60% of notar votes",
                                    state.current_slot
                                );
                                command_sender
                                    .try_send(Command::SendNotarizeCertificate(state.current_slot));
                            }
                        }
                        if !state.alpenglow_state.block_finalized {
                            if state.alpenglow_state.notarize_stake_collected > stake_80_percent {
                                state.alpenglow_state.block_has_notar_cert = true;
                                state.alpenglow_state.block_finalized = true;
                                trace!(
                                    "{id} has finalized slot {} by observing 80% of notar votes",
                                    state.current_slot
                                );
                                command_sender
                                    .try_send(Command::SendFinalizeCertificate(state.current_slot));
                            }
                        }
                    }
                    VotorMessageType::NotarizeCertificate => {
                        if !state.alpenglow_state.block_has_notar_cert {
                            if !signature.verify(pk.as_array(), &pkt_buf[SIGNATURE_BYTES..]) {
                                trace!("Sigverify failed");
                                continue;
                            }
                            state.alpenglow_state.block_has_notar_cert = true;
                            trace!(
                                "{id} has notarized slot {} by observing notar certificate",
                                state.current_slot
                            );
                            command_sender
                                .try_send(Command::SendNotarizeCertificate(state.current_slot));
                            command_sender.try_send(Command::SendFinalize(state.current_slot));
                        }
                    }
                    VotorMessageType::Finalize => {
                        if !state.alpenglow_state.block_finalized {
                            if !signature.verify(pk.as_array(), &pkt_buf[SIGNATURE_BYTES..]) {
                                trace!("Sigverify failed");
                                continue;
                            }
                            state.alpenglow_state.finalize_stake_collected += stake;
                            if state.alpenglow_state.finalize_stake_collected > stake_60_percent {
                                state.alpenglow_state.block_finalized = true;
                                trace!(
                                    "{id} has finalized slot {} by observing finalize votes",
                                    state.current_slot
                                );
                                command_sender
                                    .try_send(Command::SendFinalizeCertificate(state.current_slot));
                            }
                        }
                    }
                    VotorMessageType::FinalizeCertificate => {
                        if !state.alpenglow_state.block_finalized {
                            if !signature.verify(pk.as_array(), &pkt_buf[SIGNATURE_BYTES..]) {
                                trace!("Sigverify failed");
                                continue;
                            }
                            state.alpenglow_state.block_finalized = true;
                            command_sender
                                .try_send(Command::SendFinalizeCertificate(state.current_slot));
                        }
                    }
                }
            }
        }
    }

    /// Sends mock packets to everyone in the cluster
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
            let (slot, votor_msg) = match command {
                Command::SendNotarize(slot) => (slot, VotorMessageType::Notarize),
                Command::SendNotarizeCertificate(slot) => {
                    (slot, VotorMessageType::NotarizeCertificate)
                }
                Command::SendFinalize(slot) => (slot, VotorMessageType::Finalize),
                Command::SendFinalizeCertificate(slot) => {
                    (slot, VotorMessageType::FinalizeCertificate)
                }
            };
            prep_and_sign_packet(
                &mut packet_buf,
                slot,
                &id,
                votor_msg,
                cluster_info.keypair().as_ref(),
            );

            // prepare addresses to send the packets
            let mut send_instructions = Vec::with_capacity(2000);
            let mut lockguard = state.lock().unwrap();

            for (peer, info) in lockguard.peers.iter() {
                send_instructions.push((packet_buf.as_slice(), info.address));
                trace!(
                    "{id}: send {votor_msg:?} for slot {slot} to {} for {peer}",
                    info.address
                );
            }
            // broadcast to everybody at once
            batch_send(&socket, send_instructions);
        }
    }

    fn report_collected_votes(peers: &HashMap<Pubkey, PeerData>, total_staked: Stake) {
        let mut total_voted_stake: [Stake; 4] = [0; 4];
        let mut total_voted_nodes: [usize; 4] = [0; 4];
        let mut total_delay_ms = [0u128; 4];
        for (pubkey, peer_data) in peers.iter() {
            for i in 0..4 {
                let Some(rel_toa) = peer_data.relative_toa[i] else {
                    continue;
                };
                total_voted_stake[i] += peer_data.stake;
                total_voted_nodes[i] += 1;
                total_delay_ms[i] +=
                    (rel_toa.as_millis().clamp(0, 800) as u128) * peer_data.stake as u128;
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
                VotorMessageType::try_from(i as u64).unwrap(), // this unwrap is ok since i is in static range
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

    pub(crate) fn signal_new_slot(&mut self, slot: Slot, interval: Slot) {
        // constraint interval to be above 3 to keep current logic working
        let interval = interval.max(3);
        let phase = slot % interval;
        // this is currently set up to work over a course of 3 slots.
        // it will only work correctly if we do not actually fork within those 3 slots, and
        // are consistently casting votes for all 3.
        match phase {
            // Clear the stats for the previous slots in preparation for the new one
            0 => {
                // prepare to receive votes for the next slot in advance
                // in case we are really late getting shreds
                self.prepare_to_receive(slot + 1);
            }
            // send Notarize votes (rest of the consensus happens based on received packets)
            1 => {
                trace!("Starting voting in slot {slot}");
                self.command_sender.send(Command::SendNotarize(slot));
            }
            // report metrics
            2 => {
                trace!("Reporting statistics for slot {}", slot - 1);
                // avoid holding locks while reporting metrics
                let (peers, total_staked) = {
                    let mut lockguard = self.state.lock().unwrap();
                    let total_staked = lockguard.total_staked;
                    let peers = lockguard.reset();
                    (peers, total_staked)
                };
                Self::report_collected_votes(&peers, total_staked);
            }
            // in case we have missed previous slot, clean up and block reception
            // but do not report stats (as they will be all garbled by now)
            _ => {
                let mut lockguard = self.state.lock().unwrap();
                // block reception of votes
                lockguard.reset();
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
    state: VotorMessageType,
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
