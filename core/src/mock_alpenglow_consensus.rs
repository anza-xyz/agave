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
    solana_signature::SIGNATURE_BYTES,
    solana_signer::Signer,
    solana_streamer::{recvmmsg::recv_mmsg, sendmmsg::batch_send},
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
    vote_relative_time: Duration,
}

struct SharedState {
    current_slot_start: Instant,
    peers: HashMap<Pubkey, Counters>,
    should_exit: bool,
    current_slot: Slot,
}

// This is a placeholder that is only used for load-testing.
// This is not representative of the actual alpenglow implementation.
pub(crate) struct MockAlpenglowConsensus {
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
    fn from_bytes(buf: &[u8]) -> &Self {
        bytemuck::from_bytes::<FakeVotePacket>(&buf[..FAKE_VOTE_HEADER_SIZE])
    }
}
const CHECK_INTERVAL: Duration = Duration::from_millis(10);

impl MockAlpenglowConsensus {
    pub(crate) fn new(
        socket: UdpSocket,
        cluster_info: Arc<ClusterInfo>,
        epoch_specs: EpochSpecs,
    ) -> Self {
        //socket.set_nonblocking(true).unwrap();
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
                let epoch_specs = epoch_specs.clone();
                move || {
                    Self::listener_thread(
                        shared_state,
                        cluster_info,
                        epoch_specs,
                        socket,
                        new_slot_rx,
                    )
                }
            }),
            sender_thread: thread::spawn(move || {
                Self::sender_thread(
                    shared_state,
                    cluster_info,
                    epoch_specs,
                    socket.clone(),
                    new_slot_rx,
                )
            }),
            send_event: new_slot_tx,
        }
    }

    /// Collects votes and prepares statistics
    fn listener_thread(
        state: Arc<Mutex<SharedState>>,
        cluster_info: Arc<ClusterInfo>,
        mut epoch_specs: EpochSpecs,
        socket: Arc<UdpSocket>,

        new_slot: Receiver<Slot>,
    ) {
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut packets: Vec<Packet> = vec![Packet::default(); 2048];
        loop {
            // must wipe all Meta records to reuse the buffer
            for p in packets.iter_mut() {
                *p.meta_mut() = Meta::default();
            }
            // recv_mmsg will auto timeout in 1 second
            let Ok(n) = recv_mmsg(&socket, &mut packets) else {
                return;
            };
            let mut lockguard = state.lock().unwrap();
            if lockguard.should_exit {
                return;
            }
            let elapsed = lockguard.current_slot_start.elapsed();
            for pkt in packets.iter().take(n) {
                if pkt.meta().size < FAKE_VOTE_HEADER_SIZE {
                    continue;
                }
                let Some(pkt_buf) = pkt.data(..) else {
                    continue;
                };
                let vote_pkt = FakeVotePacket::from_bytes(pkt_buf);
                let pk = Pubkey::new_from_array(vote_pkt.sender);
                let Some(&stake) = epoch_specs.current_epoch_staked_nodes().get(&pk) else {
                    continue;
                };
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

    /// Sends fake votes to everyone in the cluster
    /// Also resets counters for the next slot
    fn sender_thread(
        state: Arc<Mutex<SharedState>>,
        cluster_info: Arc<ClusterInfo>,
        mut epoch_specs: EpochSpecs,
        socket: Arc<UdpSocket>,
        new_slot: Receiver<Slot>,
    ) {
        let mut packet_buf = vec![0u8; 1200];
        let id = cluster_info.id();

        loop {
            let Ok(slot) = new_slot.recv_timeout(CHECK_INTERVAL) else {
                if state.lock().unwrap().should_exit {
                    return;
                }
                continue;
            };

            if slot % 20 == 0 {
                for (peer, stake) in epoch_specs.current_epoch_staked_nodes().iter() {
                    error!("Stake for {peer} is {stake}");
                }
            }
            let total_staked: u64 = epoch_specs.current_epoch_staked_nodes().values().sum();
            // prepare the packet to send and sign it
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

            // Clear the stats for the previous slots in preparation for the new one
            {
                let mut lockguard = state.lock().unwrap();
                let (total_voted, weighted_delay) = Self::count_collected_votes(&lockguard.peers);
                error!(
                    "Got {} % of total stake collected, stake-weighted delay is {}ms",
                    100.0 * total_voted as f64 / total_staked as f64,
                    weighted_delay
                );
                lockguard.peers.clear();
                lockguard.current_slot = slot;
                lockguard.current_slot_start = Instant::now();
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
                error!("Sending mock vote to {ag_addr}");
            }

            // broadcast to everybody at once
            batch_send(&socket, send_instructions);
        }
    }

    fn count_collected_votes(peers: &HashMap<Pubkey, Counters>) -> (Stake, f64) {
        let mut total_voted: Stake = 1; //start with 1 lamport to avoid zero division later
        let mut total_delay_ms = 0u128;
        for (pubkey, counters) in peers.iter() {
            total_voted += counters.stake;
            total_delay_ms += (counters.vote_relative_time.as_millis().clamp(0, 400) as u128)
                * counters.stake as u128;
        }
        let stake_weighted_delay = total_delay_ms as f64 / total_voted as f64;
        (total_voted, stake_weighted_delay)
    }

    pub(crate) fn send_fake_votes(&self, slot: Slot) {
        self.send_event.send(slot);
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.state.lock().unwrap().should_exit = true;
        self.sender_thread.join()?;
        self.listener_thread.join()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serial_test::serial;
    use solana_local_cluster::local_cluster::{LocalCluster, DEFAULT_MINT_LAMPORTS};
    use solana_native_token::LAMPORTS_PER_SOL;
    use solana_streamer::socket::SocketAddrSpace;

    #[test]
    #[serial]
    fn test_mock_alpenglow_consensus() {
        solana_logger::setup_with_default("error");
        let num_nodes = 3;
        let local = LocalCluster::new_with_equal_stakes(
            num_nodes,
            DEFAULT_MINT_LAMPORTS,
            100 * LAMPORTS_PER_SOL,
            SocketAddrSpace::Unspecified,
        );
        std::thread::sleep(Duration::from_secs(10));
    }
}
