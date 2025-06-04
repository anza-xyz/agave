//#[allow(unused_imports)]
#![allow(warnings)]
use {
    crate::consensus::Stake,
    bytemuck::{Pod, Zeroable},
    chrono::TimeDelta,
    crossbeam_channel::{bounded, Receiver, Sender},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_pubkey::{Pubkey, PUBKEY_BYTES},
    solana_signature::SIGNATURE_BYTES,
    solana_signer::Signer,
    solana_streamer::sendmmsg::batch_send,
    solana_turbine::cluster_nodes,
    std::{
        collections::HashMap,
        net::{SocketAddrV4, UdpSocket},
        sync::{atomic::AtomicBool, Arc, Mutex},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    },
};

struct Counters {
    stake: Stake,
    vote_relative_time: TimeDelta,
}

struct SharedState {
    current_slot_start: Instant,
    peers: HashMap<Pubkey, Counters>,
    should_exit: bool,
    current_slot: Slot,
}

// This is a placeholder that is only used for load-testing.
// This is not representative of the actual alpenglow implementation.
pub(crate) struct FakeAlpenglowConsensus {
    sender_thread: JoinHandle<()>,
    listener_thread: JoinHandle<()>,
    state: Arc<Mutex<SharedState>>,
    send_event: Sender<Slot>,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct FakeVotePacket {
    signature: [u8; SIGNATURE_BYTES],
    sender: [u8; PUBKEY_BYTES],
    slot_number: Slot,
}
const FAKE_VOTE_HEADER_SIZE: usize = std::mem::size_of::<FakeVotePacket>();

impl FakeVotePacket {
    fn from_bytes_mut(buf: &mut [u8]) -> &mut Self {
        bytemuck::from_bytes_mut::<FakeVotePacket>(&mut buf[..FAKE_VOTE_HEADER_SIZE])
    }
}
const CHECK_INTERVAL: Duration = Duration::from_millis(10);

impl FakeAlpenglowConsensus {
    pub(crate) fn new(socket: UdpSocket, cluster_info: Arc<ClusterInfo>) -> Self {
        let socket = Arc::new(socket);
        let (new_slot_tx, new_slot_rx) = bounded(4);
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
                let new_slot_rx = new_slot_rx.clone();
                let cluster_info = cluster_info.clone();
                move || Self::listener_thread(shared_state, cluster_info, socket, new_slot_rx)
            }),
            sender_thread: thread::spawn(move || {
                Self::sender_thread(shared_state, cluster_info, socket.clone(), new_slot_rx)
            }),
            send_event: new_slot_tx,
        }
    }

    /// Collects votes and prepares statistics
    fn listener_thread(
        state: Arc<Mutex<SharedState>>,
        cluster_info: Arc<ClusterInfo>,
        socket: Arc<UdpSocket>,

        new_slot: Receiver<Slot>,
    ) {
        let mut lockguard = state.lock().unwrap();
    }

    /// Sends fake votes to everyone in the cluster
    /// Also resets counters for the next slot
    fn sender_thread(
        state: Arc<Mutex<SharedState>>,
        cluster_info: Arc<ClusterInfo>,
        socket: Arc<UdpSocket>,
        new_slot: Receiver<Slot>,
    ) {
        let mut packet_buf = vec![0u8; 1200];
        let id = cluster_info.id();

        loop {
            let slot = match new_slot.recv_timeout(CHECK_INTERVAL) {
                Ok(slot) => slot,
                Err(_) => {
                    if state.lock().unwrap().should_exit {
                        return;
                    }
                    continue;
                }
            };
            {
                let pkt = FakeVotePacket::from_bytes_mut(&mut packet_buf);
                pkt.slot_number = slot;
                pkt.sender = *id.as_array();
                pkt.signature = [0; SIGNATURE_BYTES];
            }
            let signature = cluster_info.keypair().sign_message(&packet_buf);
            {
                let pkt = FakeVotePacket::from_bytes_mut(&mut packet_buf);
                pkt.signature = *signature.as_array();
            }
            {
                let mut lockguard = state.lock().unwrap();
                let counts = Self::count_collected_votes(&lockguard.peers);
                lockguard.peers.clear();
                lockguard.current_slot = slot;
                lockguard.current_slot_start = Instant::now();
            }
            for peer in cluster_info.all_tvu_peers()
        }
    }

    fn count_collected_votes(peers:& HashMap<Pubkey, Counters>) -> (Stake, f64) {
        let mut total_voted: Stake = 0;
        let mut total_delay_ms = 0i128;
        for (pubkey, counters) in peers.iter() {
            total_voted += counters.stake;
            total_delay_ms += (counters
                .vote_relative_time
                .num_milliseconds()
                .clamp(-400, 400) as i128)
                * counters.stake as i128;
        }
        let stake_weighted_delay = total_delay_ms as f64 / total_voted as f64;
        (total_voted, stake_weighted_delay)
    }

    pub(crate) fn send_fake_votes(&mut self, slot: Slot) {
        self.send_event.send(slot);
        //let packets: Vec<_> = (0..32).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        //let packet_refs: Vec<_> = packets.iter().map(|p| (&p[..], &addr)).collect();

        //batch_send(sock, packets)
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.sender_thread.join()?;
        self.listener_thread.join()
    }
}
