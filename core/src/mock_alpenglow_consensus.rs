#![allow(warnings)]
use {
    crate::consensus::Stake,
    bytemuck::{Pod, Zeroable},
    chrono::TimeDelta,
    crossbeam_channel::{bounded, Receiver, Sender},
    dashmap::DashMap,
    futures::future::err,
    serde::de::DeserializeOwned,
    serde_bytes::Deserialize,
    solana_clock::Slot,
    solana_gossip::{
        cluster_info::ClusterInfo,
        epoch_specs::{self, EpochSpecs},
    },
    solana_keypair::Keypair,
    solana_packet::{Meta, Packet, PACKET_DATA_SIZE},
    solana_pubkey::{Pubkey, PUBKEY_BYTES},
    solana_runtime::bank::Bank,
    solana_signature::{Signature, SIGNATURE_BYTES},
    solana_signer::Signer,
    solana_streamer::{recvmmsg::recv_mmsg, sendmmsg::batch_send},
    solana_turbine::cluster_nodes,
    static_assertions::const_assert,
    std::{
        collections::HashMap,
        io::Error,
        net::{SocketAddr, SocketAddrV4, UdpSocket},
        sync::{
            atomic::{AtomicBool, AtomicU16, Ordering},
            Arc, Mutex,
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    },
    trees::TupleTree,
};

// each mock voting round takes 3 slots to complete:
// 1. prep & enable reception
// 2. initiate voting by sending Notarize
// 3. closing vote window and reporting stats
//
// This is done to ensure we can capture votes coming earlier or later than
// our own slot start/end times.
//
// If the test is configured to vote every slot, it will be running at most 3
// concurrent processes at once in a staggered fashion. Thus we need 3 worker
// threads and 3 "state" slots at the most.
const NUM_WORKERS: usize = 3;

/// This is a placeholder that is only used for load-testing.
/// This is not representative of the actual alpenglow implementation.
pub(crate) struct MockAlpenglowConsensus {
    sender_thread: JoinHandle<()>,
    listener_thread: JoinHandle<()>,
    state: Arc<[Mutex<SharedState>; NUM_WORKERS]>, // internal state of the test
    highest_slot: Slot,                            // highest slot we have observed so far
    safety_latch_engaged: bool, // will be set to true if root bank is too far behind the slot we are voting for
    should_exit: Arc<AtomicBool>,
    verify_signatures: Arc<AtomicBool>,
    packet_size: Arc<AtomicU16>,
    // external state
    epoch_specs: EpochSpecs,
    cluster_info: Arc<ClusterInfo>,
    // control of internal threadpool that handles test timings
    slot_sender: Option<Sender<Slot>>,
    thread_pool: [JoinHandle<()>; NUM_WORKERS],
}

/// Information we hold for individual peers in the test
struct PeerData {
    stake: Stake,
    address: SocketAddr,
    relative_toa: [Option<Duration>; NUM_VOTOR_TYPES],
}

/// State machine internal state for the mock alpenglow
/// This roughly approximates the actual certificate pool behavior
#[derive(Default, Debug)]
struct AgStateMachine {
    block_notarized: bool,
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
    current_slot: Slot,
    alpenglow_state: AgStateMachine,
}

impl SharedState {
    fn reset(&mut self) -> HashMap<Pubkey, PeerData> {
        let mut peers = HashMap::with_capacity(2048);
        std::mem::swap(&mut peers, &mut self.peers);
        self.current_slot = 0;
        self.total_staked = 0;
        self.alpenglow_state = AgStateMachine::default();
        peers
    }
    fn new(current_slot: Slot) -> Self {
        Self {
            current_slot_start: Instant::now(),
            peers: HashMap::with_capacity(2048),
            current_slot,
            total_staked: 0,
            alpenglow_state: AgStateMachine::default(),
        }
    }
}

fn get_state_for_slot(states: &[Mutex<SharedState>], slot: Slot) -> &Mutex<SharedState> {
    &states[(slot % NUM_WORKERS as u64) as usize]
}

/// This is just for test, and does not represent actual alpenglow
#[derive(Copy, Clone, Debug)]
#[repr(u64)]
enum VotorMessageType {
    Notarize,
    NotarizeCertificate,
    Finalize,
    // In this mockup, this acts as both Finalize and FastFinalize certificate
    FinalizeCertificate,
    // Update NUM_VOTOR_TYPES if changing this
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
const NUM_VOTOR_TYPES: usize = 4;

/// Header of the mock vote packet.
/// Actual frames on the wire may be longer as
/// configured by the sender. Only the header is signed.
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
/// but this is deliberately overtuned to be able to model the worst case.
const MOCK_VOTE_PACKET_MAX_SIZE: usize = 1024;

impl MockVotePacketHeader {
    fn from_bytes_mut(buf: &mut [u8]) -> &mut Self {
        bytemuck::from_bytes_mut::<MockVotePacketHeader>(&mut buf[..MOCK_VOTE_HEADER_SIZE])
    }
    fn from_bytes(buf: &[u8]) -> &Self {
        bytemuck::from_bytes::<MockVotePacketHeader>(&buf[..MOCK_VOTE_HEADER_SIZE])
    }
}

/// Max number of slots we can be ahead of the root bank
/// before triggering safety latching
const MAX_TOWER_HEIGHT: Slot = 32 + 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        let shared_state = Arc::new(std::array::from_fn(|_| Mutex::new(SharedState::new(0))));
        let should_exit = Arc::new(AtomicBool::new(false));
        let verify_signatures = Arc::new(AtomicBool::new(false));
        let packet_size = Arc::new(AtomicU16::new(512));

        let (slot_sender, slot_receiver) = bounded(4);
        let thread_pool = std::array::from_fn(|_| {
            let slot_receiver = slot_receiver.clone();
            let command_sender = command_sender.clone();
            let state = shared_state.clone();
            thread::spawn(move || {
                Self::runner(slot_receiver, command_sender, state);
            })
        });

        Self {
            state: shared_state.clone(),
            listener_thread: thread::spawn({
                let shared_state = shared_state.clone();
                let should_exit = should_exit.clone();
                let verify_signatures = verify_signatures.clone();
                let socket = socket.clone();
                let my_id = cluster_info.id();
                move || {
                    Self::listener_thread(
                        shared_state,
                        should_exit,
                        verify_signatures,
                        my_id,
                        socket,
                        command_sender,
                    )
                }
            }),
            sender_thread: thread::spawn({
                let epoch_specs = epoch_specs.clone();
                let cluster_info = cluster_info.clone();
                let packet_size = packet_size.clone();
                move || {
                    Self::sender_thread(
                        shared_state,
                        packet_size,
                        cluster_info,
                        epoch_specs,
                        socket.clone(),
                        vote_command_receiver,
                    )
                }
            }),
            should_exit,
            packet_size,
            epoch_specs,
            cluster_info,
            verify_signatures,
            highest_slot: 0,
            safety_latch_engaged: false,
            slot_sender: Some(slot_sender),
            thread_pool,
        }
    }

    /// prepare to receive votes for the slot indicated
    /// This should be called in advance
    /// in case we are really late getting shreds
    fn prepare_to_receive(&mut self, slot: Slot) -> Result<(), Slot> {
        trace!(
            "{}: preparing to receive for slot {slot}",
            self.cluster_info.id()
        );
        let staked_nodes = self.epoch_specs.current_epoch_staked_nodes();

        let mut state = get_state_for_slot(self.state.as_slice(), slot)
            .lock()
            .unwrap();
        if state.current_slot != 0 {
            return Err(state.current_slot);
        }
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
                    relative_toa: [None; NUM_VOTOR_TYPES],
                },
            );
            state.total_staked += stake;
        }
        trace!(
            "Prepared for slot {slot}, total stake is {}",
            state.total_staked
        );
        Ok(())
    }

    /// Collects votes and changes states
    fn listener_thread(
        self_state: Arc<[Mutex<SharedState>; NUM_WORKERS]>,
        should_exit: Arc<AtomicBool>,
        verify_signatures: Arc<AtomicBool>,
        my_id: Pubkey,
        socket: Arc<UdpSocket>,
        command_sender: Sender<Command>,
    ) {
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        trace!("Listener thread started");
        // Set aside enough space to fetch multiple packets from the kernel per syscall
        let mut packets: Vec<Packet> = vec![Packet::default(); 1024];
        loop {
            // must wipe all Meta records to reuse the buffer
            for p in packets.iter_mut() {
                *p.meta_mut() = Meta::default();
            }

            if should_exit.load(Ordering::Relaxed) {
                return;
            }
            // recv_mmsg should timeout in 1 second
            let n = match recv_mmsg(&socket, &mut packets) {
                // we may have received no packets, in this case we can safely skip the rest
                Ok(0) => continue,
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

            let verify_signatures = verify_signatures.load(Ordering::Relaxed);
            for pkt in packets.iter().take(n) {
                if pkt.meta().size < MOCK_VOTE_HEADER_SIZE {
                    trace!("Packet too small {}", pkt.meta().size);
                    continue;
                }
                let Some(pkt_buf) = pkt.data(..) else {
                    continue;
                };
                let vote_pkt = MockVotePacketHeader::from_bytes(pkt_buf);
                let pk = Pubkey::new_from_array(vote_pkt.sender);
                let signature = Signature::from(vote_pkt.signature);
                if verify_signatures
                    && !signature.verify(
                        pk.as_array(),
                        &pkt_buf[SIGNATURE_BYTES..MOCK_VOTE_HEADER_SIZE],
                    )
                {
                    trace!("Sigverify failed");
                    continue;
                }

                let mut state = get_state_for_slot(self_state.as_slice(), vote_pkt.slot_number)
                    .lock()
                    .unwrap();

                if vote_pkt.slot_number != state.current_slot {
                    trace!(
                        "Packet does not have matching slot number {} != {}",
                        vote_pkt.slot_number,
                        state.current_slot
                    );
                    continue;
                }

                let elapsed = state.current_slot_start.elapsed();

                let stake_60_percent = (state.total_staked as f64 * 0.6) as Stake;
                let stake_80_percent = (state.total_staked as f64 * 0.8) as Stake;
                let Ok(votor_msg) = VotorMessageType::try_from(vote_pkt.state) else {
                    continue;
                };
                let Some(peer_info) = state.peers.get_mut(&pk) else {
                    continue;
                };
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
                        state.alpenglow_state.notarize_stake_collected += stake;
                        trace!(
                            "{my_id}:{} of {} Notarize stake collected",
                            state.alpenglow_state.notarize_stake_collected,
                            stake_60_percent
                        );
                        if !state.alpenglow_state.block_notarized
                            && state.alpenglow_state.notarize_stake_collected >= stake_60_percent
                        {
                            state.alpenglow_state.block_notarized = true;
                            trace!(
                                "{my_id} has notarized slot {} by observing 60% of notar votes",
                                state.current_slot
                            );
                            command_sender
                                .try_send(Command::SendNotarizeCertificate(state.current_slot));
                            command_sender.try_send(Command::SendFinalize(state.current_slot));
                        }
                        if !state.alpenglow_state.block_finalized
                            && state.alpenglow_state.notarize_stake_collected >= stake_80_percent
                        {
                            state.alpenglow_state.block_finalized = true;
                            trace!(
                                "{my_id} has finalized slot {} by observing 80% of notar votes",
                                state.current_slot
                            );
                            command_sender
                                .try_send(Command::SendFinalizeCertificate(state.current_slot));
                        }
                    }
                    VotorMessageType::NotarizeCertificate => {
                        if !state.alpenglow_state.block_notarized {
                            state.alpenglow_state.block_notarized = true;
                            trace!(
                                "{my_id} has notarized slot {} by observing notar certificate",
                                state.current_slot
                            );
                            command_sender
                                .try_send(Command::SendNotarizeCertificate(state.current_slot));
                            command_sender.try_send(Command::SendFinalize(state.current_slot));
                        }
                    }
                    VotorMessageType::Finalize => {
                        state.alpenglow_state.finalize_stake_collected += stake;
                        trace!(
                            "{my_id}:{} of {} Finalize stake collected",
                            state.alpenglow_state.finalize_stake_collected,
                            stake_60_percent
                        );
                        if !state.alpenglow_state.block_finalized
                            && state.alpenglow_state.finalize_stake_collected >= stake_60_percent
                        {
                            state.alpenglow_state.block_finalized = true;
                            trace!(
                                "{my_id} has finalized slot {} by observing finalize votes",
                                state.current_slot
                            );
                            command_sender
                                .try_send(Command::SendFinalizeCertificate(state.current_slot));
                        }
                    }
                    VotorMessageType::FinalizeCertificate => {
                        if !state.alpenglow_state.block_finalized {
                            state.alpenglow_state.block_finalized = true;
                            trace!(
                                "{my_id} has finalized slot {} by observing finalize certificate",
                                state.current_slot
                            );
                            command_sender
                                .try_send(Command::SendFinalizeCertificate(state.current_slot));
                        }
                    }
                }
            }
        }
    }

    fn lockdown(&self) {
        for state in self.state.iter() {
            let mut lockguard = state.lock().unwrap();
            // block reception of votes
            lockguard.reset();
        }
    }

    /// Sends mock packets to everyone in the cluster
    fn sender_thread(
        state: Arc<[Mutex<SharedState>; NUM_WORKERS]>,
        packet_size: Arc<AtomicU16>,
        cluster_info: Arc<ClusterInfo>,
        mut epoch_specs: EpochSpecs,
        socket: Arc<UdpSocket>,
        command: Receiver<Command>,
    ) {
        let mut packet_buf = vec![0u8; MOCK_VOTE_PACKET_MAX_SIZE];
        let id = cluster_info.id();
        for command in command.iter() {
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
            let packet_size = (packet_size.load(Ordering::Relaxed) as usize)
                .clamp(MOCK_VOTE_HEADER_SIZE, MOCK_VOTE_PACKET_MAX_SIZE);

            prep_and_sign_packet(
                &mut packet_buf,
                slot,
                votor_msg,
                cluster_info.keypair().as_ref(),
            );

            // prepare addresses to send the packets
            let mut send_instructions = Vec::with_capacity(3072); // we have ~2500 validators in testnet
            {
                let mut state = get_state_for_slot(state.as_slice(), slot).lock().unwrap();
                // check if our task was aborted, avoid sending if it was.
                if state.current_slot != slot {
                    return;
                }

                for (peer, info) in state.peers.iter() {
                    send_instructions.push((&packet_buf[0..packet_size], info.address));
                    trace!(
                        "{id}: send {votor_msg:?} for slot {slot} to {} for {peer}",
                        info.address
                    );
                }
            }
            // broadcast to everybody at once
            batch_send(&socket, send_instructions);
        }
    }

    fn check_conditions_to_vote(&mut self, interval: u64, slot: Slot, root_slot: Slot) -> bool {
        if interval > 0 {
            if self.safety_latch_engaged {
                datapoint_info!(
                    "mock_alpenglow",
                    ("safety_latch", 1, i64),
                    ("slot", slot, i64),
                );
                debug!("Alpenglow test disabled due to safety latch");
                return false;
            }
            debug!("Alpenglow test interval set to {} slots", interval);
        } else {
            self.safety_latch_engaged = false;
            debug!("Alpenglow test disabled by on-chain config");
            return false;
        }

        // ensure we do not start process for a slot which is "in the past"
        if slot <= self.highest_slot {
            trace!(
                "Skipping AG logic for slot {slot}, current highest slot is {}",
                self.highest_slot
            );
            return false;
        }

        // If we fall too far behind and can not root banks, engage safety latch to stop the test
        // and keep it stopped no matter what the config says
        if root_slot + MAX_TOWER_HEIGHT < slot {
            error!(
                "root bank is too far behind ({} vs {slot}). safety latch triggered, test is disabled",
                self.highest_slot
            );
            self.safety_latch_engaged = true;
            self.lockdown();
            datapoint_info!(
                "mock_alpenglow",
                ("safety_latch", 1, i64),
                ("slot", slot, i64),
            );
            return false;
        }
        true
    }

    pub(crate) fn signal_new_slot(&mut self, slot: Slot, root_bank: &Bank) {
        let Some(config) = get_test_config_from_account::<TestConfig>(root_bank) else {
            return; // no config is available => test can not run
        };
        // copy basic parameters from the config
        self.packet_size
            .store(config.packet_size, Ordering::Relaxed);
        self.verify_signatures
            .store(config.verify_signatures, Ordering::Relaxed);
        let interval = config.test_interval_slots as u64;

        if !self.check_conditions_to_vote(interval, slot, root_bank.slot()) {
            return;
        }
        self.highest_slot = slot;
        debug_assert_ne!(interval, 0, "we should have checked for this earlier");
        if slot % interval == 0 {
            // prep to receive next slot's votes
            match self.prepare_to_receive(slot) {
                Ok(_) => {
                    if let Some(slot_sender) = self.slot_sender.as_ref() {
                        if slot_sender.try_send(slot).is_err() {
                            error!("Can not initiate mock voting, all workers are busy");
                            datapoint_info!(
                                "mock_alpenglow",
                                ("runner_stuck", 1, i64),
                                ("slot", slot, i64)
                            );
                        }
                    } else {
                        return;
                    }
                }
                Err(slot) => {
                    error!("Can not initiate mock voting, all slots are busy");
                    datapoint_info!(
                        "mock_alpenglow",
                        ("runner_stuck", 2, i64),
                        ("slot", slot, i64)
                    );
                }
            }
        }
    }

    /// Runs one test for 3 slots when new slot index
    /// is sent over slot_receiver channel
    fn runner(
        slot_receiver: Receiver<Slot>,
        command_sender: Sender<Command>,
        state: Arc<[Mutex<SharedState>; NUM_WORKERS]>,
    ) {
        for slot in slot_receiver.iter() {
            // we get activated 1 slot in advance to capture votes coming
            // earlier than we have finished replay
            std::thread::sleep(Duration::from_millis(400));
            trace!("Starting voting in slot {slot}");
            command_sender.send(Command::SendNotarize(slot));
            std::thread::sleep(Duration::from_millis(400));
            // collect stats from the previous slot's voting
            let (peers, total_staked) = {
                let mut lockguard = get_state_for_slot(state.as_slice(), slot).lock().unwrap();
                // check if tasks have been aborted and do not report garbage
                if lockguard.current_slot == 0 {
                    return;
                }
                let total_staked = lockguard.total_staked;
                let peers = lockguard.reset();
                (peers, total_staked)
            };
            report_collected_votes(peers, total_staked, slot);
        }
    }

    pub(crate) fn join(mut self) -> thread::Result<()> {
        self.should_exit.store(true, Ordering::Relaxed);
        drop(self.slot_sender.take()); // drop slot_sender to cause runners to terminate
        self.listener_thread.join()?; // this exits because of the should_exit flag we have set

        for jh in self.thread_pool {
            jh.join()?; // these exit because slot_sender is dropped
        }
        self.sender_thread.join()
    }
}

fn prep_and_sign_packet(
    packet_buf: &mut [u8],
    slot: Slot,
    state: VotorMessageType,
    keypair: &Keypair,
) {
    // prepare the packet to send and sign it
    {
        let pkt = MockVotePacketHeader::from_bytes_mut(packet_buf);
        pkt.slot_number = slot;
        pkt.sender = *keypair.pubkey().as_array();
        pkt.signature = [0; SIGNATURE_BYTES];
        pkt.state = state as u64;
    }
    let signature = keypair.sign_message(&packet_buf[SIGNATURE_BYTES..MOCK_VOTE_HEADER_SIZE]);
    {
        let pkt = MockVotePacketHeader::from_bytes_mut(packet_buf);
        pkt.signature = *signature.as_array();
    }
}

// Pubkey for the account that is used to control the test via on-chain state
mod control_pubkey {
    solana_pubkey::declare_id!("9PsiyXopc2M9DMEmsEeafNHHHAUmPKe9mHYgrk6fHPyx");
}

/// Actual on-chain state that controls the mock alpenglow test
#[derive(Deserialize, Debug)]
#[repr(C)]
pub(crate) struct TestConfig {
    _version: u8,            // This is part of Record program header
    _authority: [u8; 32],    // This is part of Record program header
    test_interval_slots: u8, // 0 here means test is disabled
    verify_signatures: bool,
    packet_size: u16,
    _future_use: [u8; 16],
}

///Parse an account's content, keeping it in cache.
pub(crate) fn get_test_config_from_account<T: DeserializeOwned>(bank: &Bank) -> Option<T> {
    let data = bank
        .accounts()
        .accounts_db
        .load_account_with(&bank.ancestors, &control_pubkey::ID, |_| true)?
        .0;
    data.deserialize_data().ok()
}

fn report_collected_votes(peers: HashMap<Pubkey, PeerData>, total_staked: Stake, slot: Slot) {
    trace!("Reporting statistics for slot {}", slot);
    let (total_voted_nodes, stake_weighted_delay, percent_collected) =
        compute_stake_weighted_means(&peers, total_staked);
    datapoint_info!(
        "mock_alpenglow",
        ("total_peers", peers.len(), f64),
        ("slot", slot, i64),
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

/// Computes the vote transmission KPIs for a given slot split
/// out by votor message type. These returned KPIs are:
/// (total messages received, stake-weighted vote delays,
/// percent of stake we received a message from)
fn compute_stake_weighted_means(
    peers: &HashMap<Pubkey, PeerData>,
    total_staked: u64,
) -> (
    [usize; NUM_VOTOR_TYPES],
    [f64; NUM_VOTOR_TYPES],
    [f64; NUM_VOTOR_TYPES],
) {
    let mut total_voted_stake: [Stake; NUM_VOTOR_TYPES] = [0; NUM_VOTOR_TYPES];
    let mut total_voted_nodes: [usize; NUM_VOTOR_TYPES] = [0; NUM_VOTOR_TYPES];
    let mut total_delay_ms = [0u128; NUM_VOTOR_TYPES];
    for (pubkey, peer_data) in peers.iter() {
        for i in 0..NUM_VOTOR_TYPES {
            let Some(rel_toa) = peer_data.relative_toa[i] else {
                continue;
            };
            total_voted_stake[i] += peer_data.stake;
            total_voted_nodes[i] += 1;
            // clamping the actual observed ToA to 800 ms to prevent outliers from
            // skewing the dataset too much.
            total_delay_ms[i] +=
                (rel_toa.as_millis().clamp(0, 800) as u128) * peer_data.stake as u128;
        }
    }

    let mut stake_weighted_delay = [0f64; NUM_VOTOR_TYPES];
    let mut percent_collected = [0f64; NUM_VOTOR_TYPES];

    for i in 0..NUM_VOTOR_TYPES {
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
    (total_voted_nodes, stake_weighted_delay, percent_collected)
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            mock_alpenglow_consensus::{
                compute_stake_weighted_means, get_state_for_slot, prep_and_sign_packet, Command,
                MockAlpenglowConsensus, PeerData, SharedState, VotorMessageType,
                MOCK_VOTE_HEADER_SIZE, MOCK_VOTE_PACKET_MAX_SIZE,
            },
            repair::repair_weighted_traversal::test,
        },
        crossbeam_channel::bounded,
        solana_clock::Slot,
        solana_keypair::Keypair,
        solana_net_utils::{
            bind_in_range,
            sockets::{bind_to_localhost_unique, localhost_port_range_for_tests},
        },
        solana_pubkey::Pubkey,
        solana_signer::Signer,
        std::{
            collections::HashMap,
            net::{IpAddr, Ipv4Addr, UdpSocket},
            sync::{
                atomic::{AtomicBool, AtomicU16},
                Arc, Mutex,
            },
            thread::sleep,
            time::{Duration, Instant},
        },
    };

    #[test]
    fn test_mock_alpenglow_statemachine() {
        let test_timeout = Duration::from_secs(3);
        let max_slots = 5;
        solana_logger::setup_with("trace");
        let num_nodes = 10;
        let keypairs: Vec<Keypair> = (0..num_nodes).map(|_| Keypair::new()).collect();
        let peers: Vec<(Pubkey, UdpSocket)> = keypairs
            .iter()
            .map(|kp| (kp.pubkey(), bind_to_localhost_unique().unwrap()))
            .collect();

        let socket = Arc::new(peers[0].1.try_clone().unwrap());
        let my_id = keypairs[0].pubkey();
        let (command_sender, vote_command_receiver) = bounded(4);
        let shared_state = Arc::new(std::array::from_fn(|_| Mutex::new(SharedState::new(0))));
        let should_exit = Arc::new(AtomicBool::new(false));
        let verify_signatures = Arc::new(AtomicBool::new(false));
        let packet_size = Arc::new(AtomicU16::new(512));

        let mut packet_tx_buf = [0u8; MOCK_VOTE_PACKET_MAX_SIZE];
        let mut packet_rx_buf = packet_tx_buf;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                MockAlpenglowConsensus::listener_thread(
                    shared_state.clone(),
                    should_exit.clone(),
                    verify_signatures,
                    my_id,
                    socket,
                    command_sender,
                )
            });
            //make sure test terminates listener thread even if we panic
            scope.spawn(|| {
                for _ in 0..max_slots {
                    if should_exit.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    sleep(test_timeout);
                }
                should_exit.store(true, std::sync::atomic::Ordering::Relaxed);
            });

            let slot = 1; // fast finalize
            debug!("Slot {slot} starting");
            let peers_map = make_peer_map(peers.as_slice());
            mock_prep_rx(&shared_state, slot, peers_map);
            // make sure initial state is correct
            {
                let slot_state = get_state_for_slot(shared_state.as_slice(), slot)
                    .lock()
                    .unwrap();
                assert_eq!(slot_state.alpenglow_state.notarize_stake_collected, 0);
                assert_eq!(slot_state.alpenglow_state.block_notarized, false);
                assert_eq!(slot_state.alpenglow_state.block_finalized, false);
            }

            sleep(Duration::from_millis(1));
            // make sure we produce NotarizeCert when getting 60% of stake
            for p in 1..=6 {
                send_packet(
                    p,
                    VotorMessageType::Notarize,
                    slot,
                    &keypairs,
                    &peers,
                    &mut packet_tx_buf,
                );
            }

            // wait for the broadcasts
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendNotarizeCertificate(slot));
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendFinalize(slot));
            {
                let slot_state = get_state_for_slot(shared_state.as_slice(), slot)
                    .lock()
                    .unwrap();
                let peerdata = slot_state.peers.get(&peers[1].0).unwrap();
                assert!(peerdata.relative_toa[0].unwrap().as_millis() > 0);
                assert!(peerdata.relative_toa[0].unwrap() < test_timeout);
                assert!(peerdata.relative_toa[1].is_none());
                assert!(peerdata.relative_toa[2].is_none());
                assert!(peerdata.relative_toa[3].is_none());
                assert_eq!(slot_state.alpenglow_state.notarize_stake_collected, 6);
                assert_eq!(slot_state.alpenglow_state.block_notarized, true);
                assert_eq!(slot_state.alpenglow_state.block_finalized, false);
            }
            sleep(Duration::from_millis(1));
            // make sure we produce FinalizeCert when getting 60% of stake sending Finalize
            for p in 1..=6 {
                send_packet(
                    p,
                    VotorMessageType::Finalize,
                    slot,
                    &keypairs,
                    &peers,
                    &mut packet_tx_buf,
                );
            }
            // wait for the broadcast
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendFinalizeCertificate(slot));
            {
                let slot_state = get_state_for_slot(shared_state.as_slice(), slot)
                    .lock()
                    .unwrap();
                let peerdata = slot_state.peers.get(&peers[1].0).unwrap();
                assert!(peerdata.relative_toa[2].unwrap().as_millis() > 0);
                assert!(peerdata.relative_toa[2].unwrap() < test_timeout);
                assert!(peerdata.relative_toa[3].is_none());
                assert_eq!(slot_state.alpenglow_state.finalize_stake_collected, 6);
                assert_eq!(slot_state.alpenglow_state.block_finalized, true);
                let (total_voted_nodes, stake_weighted_delay, percent_collected) =
                    compute_stake_weighted_means(&slot_state.peers, peers.len() as u64);
                assert_eq!(total_voted_nodes[0], 6);
                assert_eq!(total_voted_nodes[1], 0);
                assert_eq!(total_voted_nodes[2], 6);
                assert_eq!(total_voted_nodes[3], 0);
                assert!(stake_weighted_delay[0] < stake_weighted_delay[2]);
                assert_eq!(stake_weighted_delay[1], 0.0);
                assert_eq!(stake_weighted_delay[3], 0.0);
            }
            // new slot new pattern (slow finalize)
            let slot = slot + 1;
            debug!("Slot {slot} starting");
            let peers_map = make_peer_map(peers.as_slice());
            mock_prep_rx(&shared_state, slot, peers_map);

            // make sure we do not NotarizeCert when getting Notar votes
            for p in 1..=5 {
                send_packet(
                    p,
                    VotorMessageType::Notarize,
                    slot,
                    &keypairs,
                    &peers,
                    &mut packet_tx_buf,
                );
            }
            sleep(Duration::from_millis(1));
            {
                let slot_state = get_state_for_slot(shared_state.as_slice(), slot)
                    .lock()
                    .unwrap();
                assert_eq!(slot_state.alpenglow_state.block_notarized, false);
                assert_eq!(slot_state.alpenglow_state.block_finalized, false);
            }

            // now we get a couple of notarize certificates
            for p in 3..=5 {
                send_packet(
                    p,
                    VotorMessageType::NotarizeCertificate,
                    slot,
                    &keypairs,
                    &peers,
                    &mut packet_tx_buf,
                );
            }

            // wait for the broadcasts
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendNotarizeCertificate(slot));
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendFinalize(slot));
            {
                let slot_state = get_state_for_slot(shared_state.as_slice(), slot)
                    .lock()
                    .unwrap();
                assert_eq!(slot_state.alpenglow_state.block_notarized, true);
                assert_eq!(slot_state.alpenglow_state.block_finalized, false);
            }
            // and the rest of Notarize votes
            for p in 6..=9 {
                send_packet(
                    p,
                    VotorMessageType::Notarize,
                    slot,
                    &keypairs,
                    &peers,
                    &mut packet_tx_buf,
                );
            }
            // wait for the broadcast
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendFinalizeCertificate(slot));
            {
                let slot_state = get_state_for_slot(shared_state.as_slice(), slot)
                    .lock()
                    .unwrap();
                assert!(slot_state.alpenglow_state.notarize_stake_collected >= 8);
                assert_eq!(slot_state.alpenglow_state.block_notarized, true);
                assert_eq!(slot_state.alpenglow_state.block_finalized, true);
            }

            // epic packet loss we only see finalize votes
            let slot = slot + 1;
            debug!("Slot {slot} starting");
            let peers_map = make_peer_map(peers.as_slice());
            mock_prep_rx(&shared_state, slot, peers_map);

            sleep(Duration::from_millis(1));
            for p in 1..=6 {
                send_packet(
                    p,
                    VotorMessageType::Finalize,
                    slot,
                    &keypairs,
                    &peers,
                    &mut packet_tx_buf,
                );
            }
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendFinalizeCertificate(slot));
            {
                let slot_state = get_state_for_slot(shared_state.as_slice(), slot)
                    .lock()
                    .unwrap();
                assert_eq!(slot_state.alpenglow_state.notarize_stake_collected, 0);
                assert_eq!(slot_state.alpenglow_state.finalize_stake_collected, 6);
                assert_eq!(slot_state.alpenglow_state.block_notarized, false);
                assert_eq!(slot_state.alpenglow_state.block_finalized, true);
                let (total_voted_nodes, stake_weighted_delay, percent_collected) =
                    compute_stake_weighted_means(&slot_state.peers, peers.len() as u64);
                assert!(stake_weighted_delay[2] > 0.0);
            }

            // epic packet loss we only see certs
            let slot = slot + 1;

            debug!("Slot {slot} starting");
            let peers_map = make_peer_map(peers.as_slice());
            mock_prep_rx(&shared_state, slot, peers_map);

            // now we get a notarize certificate
            send_packet(
                3,
                VotorMessageType::NotarizeCertificate,
                slot,
                &keypairs,
                &peers,
                &mut packet_tx_buf,
            );
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendNotarizeCertificate(slot));
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendFinalize(slot));

            {
                let slot_state = get_state_for_slot(shared_state.as_slice(), slot)
                    .lock()
                    .unwrap();
                assert_eq!(slot_state.alpenglow_state.notarize_stake_collected, 0);
                assert_eq!(slot_state.alpenglow_state.finalize_stake_collected, 0);
                assert_eq!(slot_state.alpenglow_state.block_notarized, true);
                assert_eq!(slot_state.alpenglow_state.block_finalized, false);
            }
            // and a Finalize cert
            send_packet(
                6,
                VotorMessageType::FinalizeCertificate,
                slot,
                &keypairs,
                &peers,
                &mut packet_tx_buf,
            );
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, Command::SendFinalizeCertificate(slot));
            {
                let slot_state = get_state_for_slot(shared_state.as_slice(), slot)
                    .lock()
                    .unwrap();
                assert_eq!(slot_state.alpenglow_state.notarize_stake_collected, 0);
                assert_eq!(slot_state.alpenglow_state.finalize_stake_collected, 0);
                assert_eq!(slot_state.alpenglow_state.block_notarized, true);
                assert_eq!(slot_state.alpenglow_state.block_finalized, true);
            }
            assert!(
                slot <= max_slots,
                "max_slots should match actual test length to prevent CI from flaking"
            );
            should_exit.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    fn send_packet(
        from_peer: usize,
        votor_message: VotorMessageType,
        slot: u64,
        keypairs: &Vec<Keypair>,
        peers: &Vec<(Pubkey, UdpSocket)>,
        packet_buf: &mut [u8],
    ) {
        prep_and_sign_packet(packet_buf, slot, votor_message, &keypairs[from_peer]);
        peers[from_peer]
            .1
            .send_to(
                &packet_buf[0..MOCK_VOTE_HEADER_SIZE],
                peers[0].1.local_addr().unwrap(),
            )
            .unwrap();
    }

    fn mock_prep_rx(
        state: &[Mutex<SharedState>; 3],
        slot: Slot,
        peer_map: HashMap<Pubkey, PeerData>,
    ) {
        let mut state = get_state_for_slot(state.as_slice(), slot).lock().unwrap();
        state.reset();
        state.current_slot = slot;
        state.current_slot_start = Instant::now();
        state.total_staked = peer_map.len() as u64;
        state.peers = peer_map;
    }

    fn make_peer_map(sockets: &[(Pubkey, UdpSocket)]) -> HashMap<Pubkey, PeerData> {
        let mut result = HashMap::new();
        for (i, (peer, socket)) in sockets.iter().enumerate() {
            result.insert(
                *peer,
                PeerData {
                    stake: 1,
                    address: socket.local_addr().unwrap(),
                    relative_toa: [None; 4],
                },
            );
        }
        result
    }
}
