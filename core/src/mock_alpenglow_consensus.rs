#![allow(clippy::arithmetic_side_effects)]
use {
    crate::consensus::Stake,
    bytemuck::{Pod, Zeroable},
    crossbeam_channel::{bounded, Receiver, Sender},
    serde::{de::DeserializeOwned, Deserialize, Serialize},
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_gossip::{cluster_info::ClusterInfo, epoch_specs::EpochSpecs},
    solana_keypair::Keypair,
    solana_packet::{Meta, Packet},
    solana_pubkey::{Pubkey, PUBKEY_BYTES},
    solana_runtime::bank::Bank,
    solana_signature::SIGNATURE_BYTES,
    solana_signer::Signer,
    solana_streamer::{recvmmsg::recv_mmsg, sendmmsg::batch_send},
    std::{
        collections::HashMap,
        iter::once,
        net::{SocketAddr, UdpSocket},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    },
};

// This is a mockup of optimistic Alpenglow voting (no skips)
// Each mock voting round takes 3 slots to complete:
// 1. prep & enable reception
// 2. initiate voting by sending Notarize,
// 3. closing vote window and finalizing stats
//
// This is done to ensure we can capture votes coming earlier or later than
// our own slot start/end times.
// we run VOTES_IN_A_ROW voting rounds every N slots, where N is controlled on-chain.

/// number of voting rounds
const NUM_VOTE_ROUNDS: Slot = 4;

/// rough upper bound of number of testnet validators to avoid allocations
const NUM_TESTNET_VALIDATORS: usize = 1024 * 3;

// Configure maximal transmission rate to be ~ 100 KPPS.
// On average we have ~2000 destinations, so overall broadcast should
// take at most 20 ms.
/// Max packets to send in one batch
const MAX_PACKETS_PER_BATCH: usize = 200;
/// Interval between batches
const PACING_INTERVAL: Duration = Duration::from_millis(2);

/// This is a placeholder that is only used for load-testing.
/// This is not representative of the actual alpenglow implementation.
pub(crate) struct MockAlpenglowConsensus {
    sender_thread: JoinHandle<()>,   // thread that sends packets
    listener_thread: JoinHandle<()>, // thread that listens for votes and updates statemachine
    runner_thread: JoinHandle<()>,   // thread that signals others to perform voting tasks
    state: Arc<StateArray>,          // internal state of the test for each round
    highest_slot: Slot,              // highest slot we have observed so far
    should_exit: Arc<AtomicBool>,
    // external state
    epoch_specs: EpochSpecs,
    cluster_info: Arc<ClusterInfo>,
    // control of internal threadpool that handles test timings
    slot_sender: Option<Sender<Slot>>,
}

/// Information we hold for individual peers in the test
struct PeerData {
    stake: Stake,
    address: SocketAddr,
    relative_time_of_arrival: [Option<Duration>; NUM_VOTOR_TYPES],
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
/// only ever does anything exactly 3 times per slot for ~1ms each
struct SharedState {
    current_slot_start: Instant,
    peers: HashMap<Pubkey, PeerData>,
    total_staked: Stake,
    current_slot: Slot,
    alpenglow_state: AgStateMachine,
}

type StateArray = [Mutex<SharedState>; NUM_VOTE_ROUNDS as usize];

impl SharedState {
    fn reset(&mut self) -> HashMap<Pubkey, PeerData> {
        let mut peers = HashMap::with_capacity(NUM_TESTNET_VALIDATORS);
        std::mem::swap(&mut peers, &mut self.peers);
        self.current_slot = 0;
        self.total_staked = 0;
        self.alpenglow_state = AgStateMachine::default();
        peers
    }

    fn new(current_slot: Slot) -> Self {
        Self {
            current_slot_start: Instant::now(),
            peers: HashMap::with_capacity(NUM_TESTNET_VALIDATORS),
            current_slot,
            total_staked: 0,
            alpenglow_state: AgStateMachine::default(),
        }
    }

    fn available(&self) -> bool {
        self.current_slot == 0
    }

    fn is_ready_for_slot(&self, slot: Slot) -> bool {
        self.current_slot == slot
    }
}

const ONE_SLOT: Duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);

fn get_state_for_slot_index(states: &StateArray, slot: Slot) -> &Mutex<SharedState> {
    &states[(slot % NUM_VOTE_ROUNDS) as usize]
}

/// This is just for test, and does not represent actual alpenglow
#[derive(Copy, Clone, Debug)]
#[repr(u64)]
enum VotorMessageType {
    Notarize,
    // we can glue these since this mock does not implement skips
    NotarizeCertificateAndFinalize,
    FinalizeCertificate,
    // Update NUM_VOTOR_TYPES if changing this
}

impl TryFrom<u64> for VotorMessageType {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Notarize),
            1 => Ok(Self::NotarizeCertificateAndFinalize),
            2 => Ok(Self::FinalizeCertificate),
            _ => Err(()),
        }
    }
}
const NUM_VOTOR_TYPES: usize = 3;

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

/// Max number of slots we can be ahead of the root bank
const MAX_TOWER_HEIGHT: Slot = 32 + 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendCommand {
    Notarize(Slot),
    NotarizeCertificateAndFinalize(Slot),
    FinalizeCertificate(Slot),
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

        let (slot_sender, slot_receiver) = bounded(1);
        let runner_thread = {
            let slot_receiver = slot_receiver.clone();
            let command_sender = command_sender.clone();
            let state = shared_state.clone();
            thread::spawn(move || {
                Self::runner(slot_receiver, command_sender, state);
            })
        };

        Self {
            state: shared_state.clone(),
            listener_thread: thread::spawn({
                let shared_state = shared_state.clone();
                let should_exit = should_exit.clone();
                let socket = socket.clone();
                let my_id = cluster_info.id();
                move || {
                    Self::listener_thread(shared_state, should_exit, my_id, socket, command_sender)
                }
            }),
            sender_thread: thread::spawn({
                let cluster_info = cluster_info.clone();
                move || {
                    Self::sender_thread(
                        shared_state,
                        cluster_info,
                        socket.clone(),
                        vote_command_receiver,
                    )
                }
            }),
            runner_thread,
            should_exit,
            epoch_specs,
            cluster_info,
            highest_slot: 0,
            slot_sender: Some(slot_sender),
        }
    }

    /// prepare to receive votes for the slot indicated
    /// This should be called in advance
    /// in case we are really late getting shreds
    fn prepare_to_receive(&mut self, slot: Slot, slot_start: Instant) -> Result<(), Slot> {
        trace!(
            "{}: preparing to receive for slot {slot}",
            self.cluster_info.id()
        );
        let staked_nodes = self.epoch_specs.current_epoch_staked_nodes();

        let mut state = get_state_for_slot_index(&self.state, slot).lock().unwrap();
        if !state.available() {
            return Err(state.current_slot);
        }
        state.current_slot = slot;
        state.current_slot_start = slot_start;
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
                    stake,
                    address: ag_addr,
                    relative_time_of_arrival: [None; NUM_VOTOR_TYPES],
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
        self_state: Arc<StateArray>,
        should_exit: Arc<AtomicBool>,
        my_id: Pubkey,
        socket: Arc<UdpSocket>,
        command_sender: Sender<SendCommand>,
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
                            error!(
                                "Got error {:?} in mock alpenglow RX socket operation, exiting \
                                 thread",
                                e.raw_os_error()
                            );
                            return;
                        }
                    }
                }
            };

            for pkt in packets.iter().take(n) {
                if pkt.meta().size < MOCK_VOTE_HEADER_SIZE {
                    trace!("Packet too small {}", pkt.meta().size);
                    continue;
                }
                let sender = SocketAddr::new(pkt.meta().addr, pkt.meta().port);
                let Some(pkt_buf) = pkt.data(..) else {
                    continue;
                };
                let vote_pkt = MockVotePacketHeader::from_bytes(pkt_buf);
                let pk = Pubkey::new_from_array(vote_pkt.sender);
                if vote_pkt.signature != SIGNATURE {
                    trace!("Sigverify failed");
                    continue;
                }

                let mut state = get_state_for_slot_index(&self_state, vote_pkt.slot_number)
                    .lock()
                    .unwrap();

                if !state.is_ready_for_slot(vote_pkt.slot_number) {
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
                if sender != peer_info.address {
                    // check if sender socket matches
                    continue;
                }
                trace!(
                    "RX slot {}: {:?} from {}",
                    vote_pkt.slot_number,
                    votor_msg,
                    pk
                );
                let toa = &mut peer_info.relative_time_of_arrival[votor_msg as u64 as usize];
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
                            let _ = command_sender.try_send(
                                SendCommand::NotarizeCertificateAndFinalize(state.current_slot),
                            );
                        }
                        if !state.alpenglow_state.block_finalized
                            && state.alpenglow_state.notarize_stake_collected >= stake_80_percent
                        {
                            state.alpenglow_state.block_finalized = true;
                            trace!(
                                "{my_id} has finalized slot {} by observing 80% of notar votes",
                                state.current_slot
                            );
                            let _ = command_sender
                                .try_send(SendCommand::FinalizeCertificate(state.current_slot));
                        }
                    }
                    VotorMessageType::NotarizeCertificateAndFinalize => {
                        if !state.alpenglow_state.block_notarized {
                            state.alpenglow_state.block_notarized = true;
                            trace!(
                                "{my_id} has notarized slot {} by observing notar certificate",
                                state.current_slot
                            );
                            let _ = command_sender.try_send(
                                SendCommand::NotarizeCertificateAndFinalize(state.current_slot),
                            );
                        }
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
                            let _ = command_sender
                                .try_send(SendCommand::FinalizeCertificate(state.current_slot));
                        }
                    }
                    VotorMessageType::FinalizeCertificate => {
                        if !state.alpenglow_state.block_finalized {
                            state.alpenglow_state.block_finalized = true;
                            trace!(
                                "{my_id} has finalized slot {} by observing finalize certificate",
                                state.current_slot
                            );
                            let _ = command_sender
                                .try_send(SendCommand::FinalizeCertificate(state.current_slot));
                        }
                    }
                }
            }
        }
    }

    /// Sends mock packets to everyone in the cluster
    fn sender_thread(
        state: Arc<StateArray>,
        cluster_info: Arc<ClusterInfo>,
        socket: Arc<UdpSocket>,
        command: Receiver<SendCommand>,
    ) {
        let mut packet_buf = vec![0u8; MOCK_VOTE_PACKET_SIZE];
        let id = cluster_info.id();
        for command in command.iter() {
            let (slot, votor_msg) = match command {
                SendCommand::Notarize(slot) => (slot, VotorMessageType::Notarize),
                SendCommand::NotarizeCertificateAndFinalize(slot) => {
                    (slot, VotorMessageType::NotarizeCertificateAndFinalize)
                }
                SendCommand::FinalizeCertificate(slot) => {
                    (slot, VotorMessageType::FinalizeCertificate)
                }
            };

            prep_and_sign_packet(
                &mut packet_buf,
                slot,
                votor_msg,
                cluster_info.keypair().as_ref(),
            );

            // prepare addresses to send the packets
            let mut send_instructions = Vec::with_capacity(NUM_TESTNET_VALIDATORS); // we have ~2500 validators in testnet
            {
                let state = get_state_for_slot_index(&state, slot).lock().unwrap();
                // check if our task was aborted, avoid sending if it was.
                if !state.is_ready_for_slot(slot) {
                    return;
                }

                for (peer, info) in state.peers.iter() {
                    send_instructions.push((&packet_buf, info.address));
                    trace!(
                        "{id}: send {votor_msg:?} for slot {slot} to {} for {peer}",
                        info.address
                    );
                }
            }
            // broadcast to everybody, but in small batches to avoid correlated packet bursts
            for batch in send_instructions.as_slice().chunks(MAX_PACKETS_PER_BATCH) {
                // this does not clone the Vec with bytes, only the tuples with references
                let _ = batch_send(&socket, batch.iter().copied());
                thread::sleep(PACING_INTERVAL);
            }
        }
    }

    fn check_conditions_to_vote(&mut self, slot: Slot, root_bank: &Bank) -> bool {
        // ensure we do not start process for a slot which is "in the past"
        if slot <= self.highest_slot {
            trace!(
                "Skipping AG logic for slot {slot}, current highest slot is {}",
                self.highest_slot
            );
            return false;
        }
        self.highest_slot = slot;

        let Some(config) = get_test_config_from_account::<TestConfig>(root_bank) else {
            trace!(
                "Skipping AG logic for slot {slot}, onchain config is not available {}",
                self.highest_slot
            );
            return false; // no config is available => test can not run
        };
        let interval = config.test_interval_slots as u64;
        if interval <= NUM_VOTE_ROUNDS + 1 {
            trace!("Alpenglow voting is disabled",);
            return false;
        }

        let root_slot = root_bank.slot();
        // If we fall too far behind and can not root banks, engage safety latch to stop the test
        // and keep it stopped no matter what the config says
        if root_slot + MAX_TOWER_HEIGHT < slot {
            error!(
                "root slot ({root_slot}) is too far behind vote slot ({slot}), test will not run",
            );
            return false;
        }

        slot.is_multiple_of(interval)
    }

    pub(crate) fn signal_new_slot(&mut self, slot: Slot, root_bank: &Bank) {
        if !self.check_conditions_to_vote(slot, root_bank) {
            return;
        }
        {
            let mut slot_start = Instant::now();
            for s in slot..slot + NUM_VOTE_ROUNDS {
                if self.prepare_to_receive(s, slot_start).is_err() {
                    error!("Can not initiate mock voting, slot {s} was not released");
                    datapoint_info!("mock_alpenglow", ("runner_stuck", 2, i64), ("slot", s, i64));
                }
                slot_start += ONE_SLOT;
            }
        }

        if let Some(slot_sender) = self.slot_sender.as_ref() {
            if slot_sender.try_send(slot).is_err() {
                error!("Can not initiate mock voting, worker is busy");
                datapoint_info!(
                    "mock_alpenglow",
                    ("runner_stuck", 1, i64),
                    ("slot", slot, i64)
                );
            }
        }
    }

    /// Runs test for 4 slots in a row when new slot index
    /// is sent over slot_receiver channel
    fn runner(
        slot_receiver: Receiver<Slot>,
        command_sender: Sender<SendCommand>,
        state: Arc<StateArray>,
    ) {
        for start_slot in slot_receiver.iter() {
            let slot_range = start_slot..(start_slot + NUM_VOTE_ROUNDS);
            let vote_slots = slot_range.clone().map(Some).chain(once(None));
            let report_slots = once(None).chain(slot_range.map(Some));
            for (vote_slot, report_slot) in vote_slots.zip(report_slots) {
                // we get activated 1 slot in advance to capture votes coming
                // earlier than we have finished replay
                std::thread::sleep(ONE_SLOT);
                if let Some(slot) = vote_slot {
                    trace!("Starting voting in slot {slot}");
                    let _ = command_sender.send(SendCommand::Notarize(slot));
                }
                if let Some(slot) = report_slot {
                    // collect stats from the previous slot's voting
                    let (peers, total_staked) = {
                        let mut state_for_slot_index =
                            get_state_for_slot_index(&state, slot).lock().unwrap();
                        let state_slot = state_for_slot_index.current_slot;
                        let total_staked = state_for_slot_index.total_staked;
                        let peers = state_for_slot_index.reset();
                        // check if state is for correct slot to not report garbage
                        if state_slot != slot {
                            continue;
                        }
                        (peers, total_staked)
                    };
                    report_collected_votes(peers, total_staked, slot);
                }
            }
        }
    }

    pub(crate) fn join(mut self) -> thread::Result<()> {
        self.should_exit.store(true, Ordering::Relaxed);
        drop(self.slot_sender.take()); // drop slot_sender to cause runners to terminate
        self.listener_thread.join()?; // this exits because of the should_exit flag we have set
        self.runner_thread.join()?; // this exits because slot_sender is dropped
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
    {
        let pkt = MockVotePacketHeader::from_bytes_mut(packet_buf);
        pkt.signature = SIGNATURE;
    }
}

const SIGNATURE: [u8; SIGNATURE_BYTES] = [7u8; SIGNATURE_BYTES];

fn report_collected_votes(peers: HashMap<Pubkey, PeerData>, total_staked: Stake, slot: Slot) {
    trace!("Reporting statistics for slot {slot}");
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
        ("packets_collected_finalize_cert", total_voted_nodes[2], f64),
        (
            "percent_stake_collected_finalize_cert",
            percent_collected[2],
            f64
        ),
        (
            "weighted_delay_ms_finalize_cert",
            stake_weighted_delay[2],
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
    for (_pubkey, peer_data) in peers.iter() {
        for i in 0..NUM_VOTOR_TYPES {
            let Some(rel_toa) = peer_data.relative_time_of_arrival[i] else {
                continue;
            };
            total_voted_stake[i] += peer_data.stake;
            total_voted_nodes[i] += 1;
            // clamping the actual observed ToA to 800 ms to prevent outliers from
            // skewing the dataset too much.
            total_delay_ms[i] += rel_toa.as_millis().clamp(0, 800) * peer_data.stake as u128;
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

// Pubkey for the account that is used to control the test via on-chain state
mod control_pubkey {
    solana_pubkey::declare_id!("9PsiyXopc2M9DMEmsEeafNHHHAUmPKe9mHYgrk6fHPyx");
}

/// Actual on-chain state that controls the mock alpenglow test
#[derive(Serialize, Deserialize, Debug, Default)] // Serialize is needed for tests only
#[repr(C)]
pub(crate) struct TestConfig {
    _version: u8,             // This is part of Record program header
    _authority: [u8; 32],     // This is part of Record program header
    test_interval_slots: u16, // 0 here means test is disabled
    _packet_size: u16,
    _future_use: [u8; 16],
}

///Parse an account's content, keeping it in cache.
fn get_test_config_from_account<T: DeserializeOwned>(bank: &Bank) -> Option<T> {
    let data = bank
        .accounts()
        .accounts_db
        .load_account_with(&bank.ancestors, &control_pubkey::ID, true)?
        .0;
    data.deserialize_data().ok()
}

#[cfg(test)]
mod tests {
    use {
        crate::mock_alpenglow_consensus::{
            compute_stake_weighted_means, get_state_for_slot_index, prep_and_sign_packet,
            MockAlpenglowConsensus, PeerData, SendCommand, SharedState, StateArray, TestConfig,
            VotorMessageType, MOCK_VOTE_HEADER_SIZE, MOCK_VOTE_PACKET_SIZE, NUM_VOTOR_TYPES,
        },
        crossbeam_channel::bounded,
        solana_clock::Slot,
        solana_keypair::Keypair,
        solana_net_utils::sockets::bind_to_localhost_unique,
        solana_pubkey::Pubkey,
        solana_signer::Signer,
        std::{
            collections::HashMap,
            net::UdpSocket,
            sync::{atomic::AtomicBool, Arc, Mutex},
            thread::sleep,
            time::{Duration, Instant},
        },
    };

    #[test]
    fn test_record_size() {
        assert_eq!(
            bincode::serialized_size(&TestConfig::default()).unwrap(),
            53
        );
    }

    #[test]
    fn test_mock_alpenglow_statemachine() {
        let test_timeout = Duration::from_secs(3);
        let max_slots = 5;
        agave_logger::setup_with("trace");
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

        let mut packet_tx_buf = [0u8; MOCK_VOTE_PACKET_SIZE];
        std::thread::scope(|scope| {
            scope.spawn(|| {
                MockAlpenglowConsensus::listener_thread(
                    shared_state.clone(),
                    should_exit.clone(),
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
                let slot_state = get_state_for_slot_index(&shared_state, slot)
                    .lock()
                    .unwrap();
                assert_eq!(slot_state.alpenglow_state.notarize_stake_collected, 0);
                assert!(!slot_state.alpenglow_state.block_notarized);
                assert!(!slot_state.alpenglow_state.block_finalized);
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
            assert_eq!(cmd, SendCommand::NotarizeCertificateAndFinalize(slot));
            {
                let slot_state = get_state_for_slot_index(&shared_state, slot)
                    .lock()
                    .unwrap();
                let peerdata = slot_state.peers.get(&peers[1].0).unwrap();
                assert!(peerdata.relative_time_of_arrival[0].unwrap().as_millis() > 0);
                assert!(peerdata.relative_time_of_arrival[0].unwrap() < test_timeout);
                assert!(peerdata.relative_time_of_arrival[1].is_none());
                assert!(peerdata.relative_time_of_arrival[2].is_none());
                assert_eq!(slot_state.alpenglow_state.notarize_stake_collected, 6);
                assert!(slot_state.alpenglow_state.block_notarized);
                assert!(!slot_state.alpenglow_state.block_finalized);
            }
            sleep(Duration::from_millis(1));
            // make sure we produce FinalizeCert when getting 60% of stake sending Finalize
            for p in 1..=6 {
                send_packet(
                    p,
                    VotorMessageType::NotarizeCertificateAndFinalize,
                    slot,
                    &keypairs,
                    &peers,
                    &mut packet_tx_buf,
                );
            }
            // wait for the broadcast
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, SendCommand::FinalizeCertificate(slot));
            {
                let slot_state = get_state_for_slot_index(&shared_state, slot)
                    .lock()
                    .unwrap();
                let peerdata = slot_state.peers.get(&peers[1].0).unwrap();
                assert!(peerdata.relative_time_of_arrival[1].unwrap().as_millis() > 0);
                assert!(peerdata.relative_time_of_arrival[1].unwrap() < test_timeout);
                assert!(peerdata.relative_time_of_arrival[2].is_none());
                assert_eq!(slot_state.alpenglow_state.finalize_stake_collected, 6);
                assert!(slot_state.alpenglow_state.block_finalized);
                let (total_voted_nodes, stake_weighted_delay, _percent_collected) =
                    compute_stake_weighted_means(&slot_state.peers, peers.len() as u64);
                assert_eq!(total_voted_nodes[0], 6);
                assert_eq!(total_voted_nodes[1], 6);
                assert!(stake_weighted_delay[0] < stake_weighted_delay[1]);
                assert_eq!(stake_weighted_delay[2], 0.0);
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
                let slot_state = get_state_for_slot_index(&shared_state, slot)
                    .lock()
                    .unwrap();
                assert!(!slot_state.alpenglow_state.block_notarized);
                assert!(!slot_state.alpenglow_state.block_finalized);
            }

            // now we get a couple of notarize certificates
            for p in 3..=5 {
                send_packet(
                    p,
                    VotorMessageType::NotarizeCertificateAndFinalize,
                    slot,
                    &keypairs,
                    &peers,
                    &mut packet_tx_buf,
                );
            }

            // wait for the broadcasts
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, SendCommand::NotarizeCertificateAndFinalize(slot));
            {
                let slot_state = get_state_for_slot_index(&shared_state, slot)
                    .lock()
                    .unwrap();
                assert!(slot_state.alpenglow_state.block_notarized);
                assert!(!slot_state.alpenglow_state.block_finalized);
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
            assert_eq!(cmd, SendCommand::FinalizeCertificate(slot));
            {
                let slot_state = get_state_for_slot_index(&shared_state, slot)
                    .lock()
                    .unwrap();
                assert!(slot_state.alpenglow_state.notarize_stake_collected >= 8);
                assert!(slot_state.alpenglow_state.block_notarized);
                assert!(slot_state.alpenglow_state.block_finalized);
            }

            // epic packet loss we only see certs
            let slot = slot + 1;

            debug!("Slot {slot} starting");
            let peers_map = make_peer_map(peers.as_slice());
            mock_prep_rx(&shared_state, slot, peers_map);

            // now we get a notarize certificate
            send_packet(
                3,
                VotorMessageType::NotarizeCertificateAndFinalize,
                slot,
                &keypairs,
                &peers,
                &mut packet_tx_buf,
            );
            let cmd = vote_command_receiver.recv_timeout(test_timeout).unwrap();
            assert_eq!(cmd, SendCommand::NotarizeCertificateAndFinalize(slot));

            {
                let slot_state = get_state_for_slot_index(&shared_state, slot)
                    .lock()
                    .unwrap();
                assert_eq!(slot_state.alpenglow_state.notarize_stake_collected, 0);
                assert_eq!(slot_state.alpenglow_state.finalize_stake_collected, 1);
                assert!(slot_state.alpenglow_state.block_notarized);
                assert!(!slot_state.alpenglow_state.block_finalized);
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
            assert_eq!(cmd, SendCommand::FinalizeCertificate(slot));
            {
                let slot_state = get_state_for_slot_index(&shared_state, slot)
                    .lock()
                    .unwrap();
                assert_eq!(slot_state.alpenglow_state.notarize_stake_collected, 0);
                assert_eq!(slot_state.alpenglow_state.finalize_stake_collected, 1);
                assert!(slot_state.alpenglow_state.block_notarized);
                assert!(slot_state.alpenglow_state.block_finalized);
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
        keypairs: &[Keypair],
        peers: &[(Pubkey, UdpSocket)],
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

    fn mock_prep_rx(state: &StateArray, slot: Slot, peer_map: HashMap<Pubkey, PeerData>) {
        let mut state = get_state_for_slot_index(state, slot).lock().unwrap();
        state.reset();
        state.current_slot = slot;
        state.current_slot_start = Instant::now();
        state.total_staked = peer_map.len() as u64;
        state.peers = peer_map;
    }

    fn make_peer_map(sockets: &[(Pubkey, UdpSocket)]) -> HashMap<Pubkey, PeerData> {
        let mut result = HashMap::new();
        for (peer, socket) in sockets {
            result.insert(
                *peer,
                PeerData {
                    stake: 1,
                    address: socket.local_addr().unwrap(),
                    relative_time_of_arrival: [None; NUM_VOTOR_TYPES],
                },
            );
        }
        result
    }
}
