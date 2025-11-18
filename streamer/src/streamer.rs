//! The `streamer` module defines a set of services for efficiently pulling data from UDP sockets.
//!

use {
    crate::{
        packet::{
            self, Packet, PacketBatch, PacketBatchRecycler, PacketRef, RecycledPacketBatch,
            PACKETS_PER_BATCH,
        },
        sendmmsg::{batch_send, SendPktsError},
    },
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender, TrySendError},
    histogram::Histogram,
    itertools::Itertools,
    solana_net_utils::{
        multihomed_sockets::{
            BindIpAddrs, CurrentSocket, FixedSocketProvider, MultihomedSocketProvider,
            SocketProvider,
        },
        SocketAddrSpace,
    },
    solana_pubkey::Pubkey,
    solana_time_utils::timestamp,
    std::{
        cmp::Reverse,
        collections::HashMap,
        net::{IpAddr, UdpSocket},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        thread::{sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};
#[cfg(unix)]
use {
    nix::poll::{PollFd, PollFlags},
    std::os::fd::AsFd,
};

pub trait ChannelSend<T>: Send + 'static {
    fn send(&self, msg: T) -> std::result::Result<(), SendError<T>>;

    fn try_send(&self, msg: T) -> std::result::Result<(), TrySendError<T>>;

    fn is_empty(&self) -> bool;

    fn len(&self) -> usize;
}

impl<T> ChannelSend<T> for Sender<T>
where
    T: Send + 'static,
{
    #[inline]
    fn send(&self, msg: T) -> std::result::Result<(), SendError<T>> {
        self.send(msg)
    }

    #[inline]
    fn try_send(&self, msg: T) -> std::result::Result<(), TrySendError<T>> {
        self.try_send(msg)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.is_empty()
    }

    #[inline]
    fn len(&self) -> usize {
        self.len()
    }
}

pub(crate) const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(1);

// Total stake and nodes => stake map
#[derive(Default)]
pub struct StakedNodes {
    stakes: Arc<HashMap<Pubkey, u64>>,
    overrides: HashMap<Pubkey, u64>,
    total_stake: u64,
    max_stake: u64,
    min_stake: u64,
}

pub type PacketBatchReceiver = Receiver<PacketBatch>;
pub type PacketBatchSender = Sender<PacketBatch>;

#[derive(Error, Debug)]
pub enum StreamerError {
    #[error("I/O error")]
    Io(#[from] std::io::Error),

    #[error("receive timeout error")]
    RecvTimeout(#[from] RecvTimeoutError),

    #[error("send packets error")]
    Send(#[from] SendError<PacketBatch>),

    #[error(transparent)]
    SendPktsError(#[from] SendPktsError),
}

pub struct StreamerReceiveStats {
    pub name: &'static str,
    pub packets_count: AtomicUsize,
    pub packet_batches_count: AtomicUsize,
    pub full_packet_batches_count: AtomicUsize,
    pub max_channel_len: AtomicUsize,
    pub num_packets_dropped: AtomicUsize,
}

impl StreamerReceiveStats {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            packets_count: AtomicUsize::default(),
            packet_batches_count: AtomicUsize::default(),
            full_packet_batches_count: AtomicUsize::default(),
            max_channel_len: AtomicUsize::default(),
            num_packets_dropped: AtomicUsize::default(),
        }
    }

    pub fn report(&self) {
        datapoint_info!(
            self.name,
            (
                "packets_count",
                self.packets_count.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "packet_batches_count",
                self.packet_batches_count.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "full_packet_batches_count",
                self.full_packet_batches_count.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "channel_len",
                self.max_channel_len.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "num_packets_dropped",
                self.num_packets_dropped.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
        );
    }
}

pub type Result<T> = std::result::Result<T, StreamerError>;

fn recv_loop<P: SocketProvider>(
    provider: &mut P,
    exit: &AtomicBool,
    packet_batch_sender: &impl ChannelSend<PacketBatch>,
    recycler: &PacketBatchRecycler,
    stats: &StreamerReceiveStats,
    coalesce: Option<Duration>,
    use_pinned_memory: bool,
    in_vote_only_mode: Option<Arc<AtomicBool>>,
    is_staked_service: bool,
) -> Result<()> {
    fn setup_socket(socket: &UdpSocket) -> Result<()> {
        // Non-unix implementation may block indefinitely due to its lack of polling support,
        // so we set a read timeout to avoid blocking indefinitely.
        #[cfg(not(unix))]
        socket.set_read_timeout(Some(SOCKET_READ_TIMEOUT))?;

        #[cfg(unix)]
        socket.set_nonblocking(true)?;

        Ok(())
    }

    let mut socket = provider.current_socket_ref();
    setup_socket(socket)?;
    #[cfg(unix)]
    let mut poll_fd = [PollFd::new(socket.as_fd(), PollFlags::POLLIN)];

    loop {
        let mut packet_batch = if use_pinned_memory {
            RecycledPacketBatch::new_with_recycler(recycler, PACKETS_PER_BATCH, stats.name)
        } else {
            RecycledPacketBatch::with_capacity(PACKETS_PER_BATCH)
        };
        packet_batch.resize(PACKETS_PER_BATCH, Packet::default());

        loop {
            // Check for exit signal, even if socket is busy
            // (for instance the leader transaction socket)
            if exit.load(Ordering::Relaxed) {
                return Ok(());
            }

            if let Some(ref in_vote_only_mode) = in_vote_only_mode {
                if in_vote_only_mode.load(Ordering::Relaxed) {
                    sleep(Duration::from_millis(1));
                    continue;
                }
            }

            #[cfg(unix)]
            let result = packet::recv_from(&mut packet_batch, socket, coalesce, &mut poll_fd);
            #[cfg(not(unix))]
            let result = packet::recv_from(&mut packet_batch, socket, coalesce);

            if let Ok(len) = result {
                if len > 0 {
                    let StreamerReceiveStats {
                        packets_count,
                        packet_batches_count,
                        full_packet_batches_count,
                        max_channel_len,
                        ..
                    } = stats;

                    packets_count.fetch_add(len, Ordering::Relaxed);
                    packet_batches_count.fetch_add(1, Ordering::Relaxed);
                    max_channel_len.fetch_max(packet_batch_sender.len(), Ordering::Relaxed);
                    if len == PACKETS_PER_BATCH {
                        full_packet_batches_count.fetch_add(1, Ordering::Relaxed);
                    }
                    packet_batch
                        .iter_mut()
                        .for_each(|p| p.meta_mut().set_from_staked_node(is_staked_service));
                    match packet_batch_sender.try_send(packet_batch.into()) {
                        Ok(_) => {}
                        Err(TrySendError::Full(_)) => {
                            stats.num_packets_dropped.fetch_add(len, Ordering::Relaxed);
                        }
                        Err(TrySendError::Disconnected(err)) => {
                            return Err(StreamerError::Send(SendError(err)))
                        }
                    }
                }
                break;
            }
        }

        if let CurrentSocket::Changed(s) = provider.current_socket() {
            socket = s;
            setup_socket(socket)?;

            #[cfg(unix)]
            {
                poll_fd = [PollFd::new(socket.as_fd(), PollFlags::POLLIN)];
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn receiver(
    thread_name: String,
    socket: Arc<UdpSocket>,
    exit: Arc<AtomicBool>,
    packet_batch_sender: impl ChannelSend<PacketBatch>,
    recycler: PacketBatchRecycler,
    stats: Arc<StreamerReceiveStats>,
    coalesce: Option<Duration>,
    use_pinned_memory: bool,
    in_vote_only_mode: Option<Arc<AtomicBool>>,
    is_staked_service: bool,
) -> JoinHandle<()> {
    Builder::new()
        .name(thread_name)
        .spawn(move || {
            let mut provider = FixedSocketProvider::new(socket);
            let _ = recv_loop(
                &mut provider,
                &exit,
                &packet_batch_sender,
                &recycler,
                &stats,
                coalesce,
                use_pinned_memory,
                in_vote_only_mode,
                is_staked_service,
            );
        })
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
pub fn receiver_atomic(
    thread_name: String,
    sockets: Arc<[UdpSocket]>,
    bind_ip_addrs: Arc<BindIpAddrs>,
    exit: Arc<AtomicBool>,
    packet_batch_sender: impl ChannelSend<PacketBatch>,
    recycler: PacketBatchRecycler,
    stats: Arc<StreamerReceiveStats>,
    coalesce: Option<Duration>,
    use_pinned_memory: bool,
    in_vote_only_mode: Option<Arc<AtomicBool>>,
    is_staked_service: bool,
) -> JoinHandle<()> {
    Builder::new()
        .name(thread_name)
        .spawn(move || {
            let mut provider = MultihomedSocketProvider::new(sockets, bind_ip_addrs);
            let _ = recv_loop(
                &mut provider,
                &exit,
                &packet_batch_sender,
                &recycler,
                &stats,
                coalesce,
                use_pinned_memory,
                in_vote_only_mode,
                is_staked_service,
            );
        })
        .unwrap()
}

#[derive(Debug, Default)]
struct SendStats {
    bytes: u64,
    count: u64,
}

#[derive(Default)]
struct StreamerSendStats {
    host_map: HashMap<IpAddr, SendStats>,
    since: Option<Instant>,
}

impl StreamerSendStats {
    fn report_stats(
        name: &'static str,
        host_map: HashMap<IpAddr, SendStats>,
        sample_duration: Option<Duration>,
    ) {
        const MAX_REPORT_ENTRIES: usize = 5;
        let sample_ms = sample_duration.map(|d| d.as_millis()).unwrap_or_default();
        let mut hist = Histogram::default();
        let mut byte_sum = 0;
        let mut pkt_count = 0;
        host_map.iter().for_each(|(_addr, host_stats)| {
            hist.increment(host_stats.bytes).unwrap();
            byte_sum += host_stats.bytes;
            pkt_count += host_stats.count;
        });

        datapoint_info!(
            name,
            ("streamer-send-sample_duration_ms", sample_ms, i64),
            ("streamer-send-host_count", host_map.len(), i64),
            ("streamer-send-bytes_total", byte_sum, i64),
            ("streamer-send-pkt_count_total", pkt_count, i64),
            (
                "streamer-send-host_bytes_min",
                hist.minimum().unwrap_or_default(),
                i64
            ),
            (
                "streamer-send-host_bytes_max",
                hist.maximum().unwrap_or_default(),
                i64
            ),
            (
                "streamer-send-host_bytes_mean",
                hist.mean().unwrap_or_default(),
                i64
            ),
            (
                "streamer-send-host_bytes_90pct",
                hist.percentile(90.0).unwrap_or_default(),
                i64
            ),
            (
                "streamer-send-host_bytes_50pct",
                hist.percentile(50.0).unwrap_or_default(),
                i64
            ),
            (
                "streamer-send-host_bytes_10pct",
                hist.percentile(10.0).unwrap_or_default(),
                i64
            ),
        );

        let num_entries = host_map.len();
        let mut entries: Vec<_> = host_map.into_iter().collect();
        if entries.len() > MAX_REPORT_ENTRIES {
            entries.select_nth_unstable_by_key(MAX_REPORT_ENTRIES, |(_addr, stats)| {
                Reverse(stats.bytes)
            });
            entries.truncate(MAX_REPORT_ENTRIES);
        }
        info!("streamer send {name} hosts: count:{num_entries} {entries:?}");
    }

    fn maybe_submit(&mut self, name: &'static str, sender: &Sender<Box<dyn FnOnce() + Send>>) {
        const SUBMIT_CADENCE: Duration = Duration::from_secs(10);
        const MAP_SIZE_REPORTING_THRESHOLD: usize = 1_000;
        let elapsed = self.since.as_ref().map(Instant::elapsed);
        if elapsed.map(|e| e < SUBMIT_CADENCE).unwrap_or_default()
            && self.host_map.len() < MAP_SIZE_REPORTING_THRESHOLD
        {
            return;
        }

        let host_map = std::mem::take(&mut self.host_map);
        let _ = sender.send(Box::new(move || {
            Self::report_stats(name, host_map, elapsed);
        }));

        *self = Self {
            since: Some(Instant::now()),
            ..Self::default()
        };
    }

    fn record(&mut self, pkt: PacketRef) {
        let ent = self.host_map.entry(pkt.meta().addr).or_default();
        ent.count += 1;
        ent.bytes += pkt.data(..).map(<[u8]>::len).unwrap_or_default() as u64;
    }
}

impl StakedNodes {
    /// Calculate the stake stats: return the new (total_stake, min_stake and max_stake) tuple
    fn calculate_stake_stats(
        stakes: &Arc<HashMap<Pubkey, u64>>,
        overrides: &HashMap<Pubkey, u64>,
    ) -> (u64, u64, u64) {
        let values = stakes
            .iter()
            .filter(|(pubkey, _)| !overrides.contains_key(pubkey))
            .map(|(_, &stake)| stake)
            .chain(overrides.values().copied())
            .filter(|&stake| stake > 0);
        let total_stake = values.clone().sum();
        let (min_stake, max_stake) = values.minmax().into_option().unwrap_or_default();
        (total_stake, min_stake, max_stake)
    }

    pub fn new(stakes: Arc<HashMap<Pubkey, u64>>, overrides: HashMap<Pubkey, u64>) -> Self {
        let (total_stake, min_stake, max_stake) = Self::calculate_stake_stats(&stakes, &overrides);
        Self {
            stakes,
            overrides,
            total_stake,
            max_stake,
            min_stake,
        }
    }

    pub fn get_node_stake(&self, pubkey: &Pubkey) -> Option<u64> {
        self.overrides
            .get(pubkey)
            .or_else(|| self.stakes.get(pubkey))
            .filter(|&&stake| stake > 0)
            .copied()
    }

    #[inline]
    pub fn total_stake(&self) -> u64 {
        self.total_stake
    }

    #[inline]
    pub(super) fn min_stake(&self) -> u64 {
        self.min_stake
    }

    #[inline]
    pub(super) fn max_stake(&self) -> u64 {
        self.max_stake
    }

    // Update the stake map given a new stakes map
    pub fn update_stake_map(&mut self, stakes: Arc<HashMap<Pubkey, u64>>) {
        let (total_stake, min_stake, max_stake) =
            Self::calculate_stake_stats(&stakes, &self.overrides);

        self.total_stake = total_stake;
        self.min_stake = min_stake;
        self.max_stake = max_stake;
        self.stakes = stakes;
    }
}

fn recv_send(
    sock: &UdpSocket,
    r: &PacketBatchReceiver,
    socket_addr_space: &SocketAddrSpace,
    stats: &mut Option<StreamerSendStats>,
) -> Result<()> {
    let timer = Duration::new(1, 0);
    let packet_batch = r.recv_timeout(timer)?;
    if let Some(stats) = stats {
        packet_batch.iter().for_each(|p| stats.record(p));
    }
    let packets = packet_batch.iter().filter_map(|pkt| {
        let addr = pkt.meta().socket_addr();
        let data = pkt.data(..)?;
        socket_addr_space.check(&addr).then_some((data, addr))
    });
    batch_send(sock, packets.collect::<Vec<_>>())?;
    Ok(())
}

pub fn recv_packet_batches(
    recvr: &PacketBatchReceiver,
) -> Result<(Vec<PacketBatch>, usize, Duration)> {
    let recv_start = Instant::now();
    let timer = Duration::new(1, 0);
    let packet_batch = recvr.recv_timeout(timer)?;
    trace!("got packets");
    let mut num_packets = packet_batch.len();
    let mut packet_batches = vec![packet_batch];
    while let Ok(packet_batch) = recvr.try_recv() {
        trace!("got more packets");
        num_packets += packet_batch.len();
        packet_batches.push(packet_batch);
    }
    let recv_duration = recv_start.elapsed();
    trace!(
        "packet batches len: {}, num packets: {}",
        packet_batches.len(),
        num_packets
    );
    Ok((packet_batches, num_packets, recv_duration))
}

pub fn responder_atomic(
    name: &'static str,
    sockets: Arc<[UdpSocket]>,
    bind_ip_addrs: Arc<BindIpAddrs>,
    r: PacketBatchReceiver,
    socket_addr_space: SocketAddrSpace,
    stats_reporter_sender: Option<Sender<Box<dyn FnOnce() + Send>>>,
) -> JoinHandle<()> {
    Builder::new()
        .name(format!("solRspndr{name}"))
        .spawn(move || {
            responder_loop(
                MultihomedSocketProvider::new(sockets, bind_ip_addrs),
                name,
                r,
                socket_addr_space,
                stats_reporter_sender,
            );
        })
        .unwrap()
}

pub fn responder(
    name: &'static str,
    sock: Arc<UdpSocket>,
    r: PacketBatchReceiver,
    socket_addr_space: SocketAddrSpace,
    stats_reporter_sender: Option<Sender<Box<dyn FnOnce() + Send>>>,
) -> JoinHandle<()> {
    Builder::new()
        .name(format!("solRspndr{name}"))
        .spawn(move || {
            responder_loop(
                FixedSocketProvider::new(sock),
                name,
                r,
                socket_addr_space,
                stats_reporter_sender,
            );
        })
        .unwrap()
}

fn responder_loop<P: SocketProvider>(
    provider: P,
    name: &'static str,
    r: PacketBatchReceiver,
    socket_addr_space: SocketAddrSpace,
    stats_reporter_sender: Option<Sender<Box<dyn FnOnce() + Send>>>,
) {
    let mut errors = 0;
    let mut last_error = None;
    let mut last_print = 0;
    let mut stats = None;

    if stats_reporter_sender.is_some() {
        stats = Some(StreamerSendStats::default());
    }

    loop {
        let sock = provider.current_socket_ref();
        if let Err(e) = recv_send(sock, &r, &socket_addr_space, &mut stats) {
            match e {
                StreamerError::RecvTimeout(RecvTimeoutError::Disconnected) => break,
                StreamerError::RecvTimeout(RecvTimeoutError::Timeout) => (),
                _ => {
                    errors += 1;
                    last_error = Some(e);
                }
            }
        }
        let now = timestamp();
        if now - last_print > 1000 && errors != 0 {
            datapoint_info!(name, ("errors", errors, i64),);
            info!("{name} last-error: {last_error:?} count: {errors}");
            last_print = now;
            errors = 0;
        }
        if let Some(ref stats_reporter_sender) = stats_reporter_sender {
            if let Some(ref mut stats) = stats {
                stats.maybe_submit(name, stats_reporter_sender);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::{
            packet::{Packet, RecycledPacketBatch, PACKET_DATA_SIZE},
            streamer::{receiver, responder},
        },
        crossbeam_channel::unbounded,
        solana_net_utils::sockets::bind_to_localhost_unique,
        solana_perf::recycler::Recycler,
        std::{
            io::{self, Write},
            sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            },
            time::Duration,
        },
    };

    fn get_packet_batches(r: PacketBatchReceiver, num_packets: &mut usize) {
        for _ in 0..10 {
            let packet_batch_res = r.recv_timeout(Duration::new(1, 0));
            if packet_batch_res.is_err() {
                continue;
            }

            *num_packets -= packet_batch_res.unwrap().len();

            if *num_packets == 0 {
                break;
            }
        }
    }

    #[test]
    fn streamer_debug() {
        write!(io::sink(), "{:?}", Packet::default()).unwrap();
        write!(io::sink(), "{:?}", RecycledPacketBatch::default()).unwrap();
    }
    #[test]
    fn streamer_send_test() {
        let read = bind_to_localhost_unique().expect("should bind reader");
        read.set_read_timeout(Some(SOCKET_READ_TIMEOUT)).unwrap();
        let addr = read.local_addr().unwrap();
        let send = bind_to_localhost_unique().expect("should bind sender");
        let exit = Arc::new(AtomicBool::new(false));
        let (s_reader, r_reader) = unbounded();
        let stats = Arc::new(StreamerReceiveStats::new("test"));
        let t_receiver = receiver(
            "solRcvrTest".to_string(),
            Arc::new(read),
            exit.clone(),
            s_reader,
            Recycler::default(),
            stats.clone(),
            Some(Duration::from_millis(1)), // coalesce
            true,
            None,
            false,
        );
        const NUM_PACKETS: usize = 5;
        let t_responder = {
            let (s_responder, r_responder) = unbounded();
            let t_responder = responder(
                "SendTest",
                Arc::new(send),
                r_responder,
                SocketAddrSpace::Unspecified,
                None,
            );
            let mut packet_batch = RecycledPacketBatch::default();
            for i in 0..NUM_PACKETS {
                let mut p = Packet::default();
                {
                    p.buffer_mut()[0] = i as u8;
                    p.meta_mut().size = PACKET_DATA_SIZE;
                    p.meta_mut().set_socket_addr(&addr);
                }
                packet_batch.push(p);
            }
            let packet_batch = PacketBatch::from(packet_batch);
            s_responder.send(packet_batch).expect("send");
            t_responder
        };

        let mut packets_remaining = NUM_PACKETS;
        get_packet_batches(r_reader, &mut packets_remaining);
        assert_eq!(packets_remaining, 0);
        exit.store(true, Ordering::Relaxed);
        assert!(stats.packet_batches_count.load(Ordering::Relaxed) >= 1);
        assert_eq!(stats.packets_count.load(Ordering::Relaxed), NUM_PACKETS);
        assert_eq!(stats.full_packet_batches_count.load(Ordering::Relaxed), 0);
        t_receiver.join().expect("join");
        t_responder.join().expect("join");
    }
}
