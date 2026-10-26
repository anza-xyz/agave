//! Measures round-trip latency and packet loss against gossip endpoints by
//! sending `Protocol::PingMessage` and matching the returned
//! `Protocol::PongMessage` by ping-token hash.
//!
//! The gossip ping responder is stateless: it replies to whatever source
//! address the ping arrived from and needs no prior knowledge of the sender.
//! So probing requires no cluster membership, no shred version and no stake --
//! only a UDP socket and a keypair to sign the pings with.

use {
    crate::{
        ping_pong::hash_ping_token,
        protocol::{GOSSIP_PING_TOKEN_SIZE, Ping, Protocol, deserialize_protocol},
    },
    itertools::Itertools,
    rand::Rng,
    solana_hash::Hash,
    solana_keypair::{Keypair, signable::Signable},
    solana_net_utils::sockets::bind_to,
    solana_perf::packet::PACKET_DATA_SIZE,
    solana_pubkey::Pubkey,
    solana_sanitize::Sanitize,
    solana_signer::Signer,
    std::{
        collections::HashMap,
        io,
        net::{IpAddr, SocketAddr, UdpSocket},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    },
};

/// How long the receive loop blocks on `recv_from` before it checks for
/// expired pings and for the sender having finished.
const RECV_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct PingProbeConfig {
    /// Local address to bind the probe socket to.
    pub bind_ip: IpAddr,
    /// Delay between successive rounds of pings.
    pub interval: Duration,
    /// A pong arriving later than this is not counted as received.
    pub timeout: Duration,
    /// Number of pings to send to each target, or `None` to probe forever.
    pub count: Option<u64>,
    /// How often to log an interim summary while the probe is running.
    pub report_interval: Duration,
}

#[derive(Clone, Debug)]
pub struct TargetStats {
    pub addr: SocketAddr,
    /// Pings successfully written to the socket.
    pub sent: u64,
    /// Pongs which matched an outstanding ping within the timeout.
    pub received: u64,
    /// Pongs from this target which matched no outstanding ping, i.e. the
    /// reply came back after the timeout had already discarded it.
    pub late: u64,
    /// Pongs from this target which failed sanitize or signature verification.
    pub invalid: u64,
    /// Pings which could not be handed to the socket at all.
    pub send_errors: u64,
    /// Round-trip times of the pongs counted in `received`.
    pub rtts: Vec<Duration>,
    /// Which identities answered on this endpoint, and how many times each.
    pub responders: HashMap<Pubkey, u64>,
}

impl TargetStats {
    fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            sent: 0,
            received: 0,
            late: 0,
            invalid: 0,
            send_errors: 0,
            rtts: Vec::new(),
            responders: HashMap::new(),
        }
    }

    /// Fraction of sent pings which were not answered within the timeout.
    pub fn loss_percent(&self) -> f64 {
        if self.sent == 0 {
            return 0.0;
        }
        100.0 * (self.sent - self.received) as f64 / self.sent as f64
    }

    /// `None` if the target never answered in time.
    pub fn rtt_summary(&self) -> Option<RttSummary> {
        RttSummary::new(&self.rtts)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RttSummary {
    pub min: Duration,
    pub p50: Duration,
    pub mean: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub stddev: Duration,
}

impl RttSummary {
    fn new(rtts: &[Duration]) -> Option<Self> {
        if rtts.is_empty() {
            return None;
        }
        let mut rtts: Vec<_> = rtts.iter().map(Duration::as_micros).collect();
        rtts.sort_unstable();
        let percentile = |p: f64| {
            let rank = (p * (rtts.len() - 1) as f64).round() as usize;
            Duration::from_micros(rtts[rank] as u64)
        };
        let mean = rtts.iter().sum::<u128>() as f64 / rtts.len() as f64;
        let variance = rtts
            .iter()
            .map(|&rtt| (rtt as f64 - mean).powi(2))
            .sum::<f64>()
            / rtts.len() as f64;
        Some(Self {
            min: Duration::from_micros(rtts[0] as u64),
            p50: percentile(0.5),
            mean: Duration::from_micros(mean as u64),
            p99: percentile(0.99),
            max: Duration::from_micros(rtts[rtts.len() - 1] as u64),
            stddev: Duration::from_micros(variance.sqrt() as u64),
        })
    }
}

struct ProbeState {
    /// Index into `stats` keyed by target address, for attributing replies.
    index: HashMap<SocketAddr, usize>,
    stats: Vec<TargetStats>,
    /// Expected pong hash -> (index into `stats`, time the ping was sent).
    outstanding: HashMap<Hash, (usize, Instant)>,
}

impl ProbeState {
    fn new(targets: &[SocketAddr]) -> Self {
        Self {
            index: targets
                .iter()
                .enumerate()
                .map(|(index, addr)| (*addr, index))
                .collect(),
            stats: targets.iter().copied().map(TargetStats::new).collect(),
            outstanding: HashMap::new(),
        }
    }

    fn record_reply(&mut self, bytes: &[u8], from: SocketAddr, timeout: Duration) {
        let Some(&index) = self.index.get(&from) else {
            return; // Not from any target we are probing.
        };
        let pong = match deserialize_protocol(bytes) {
            Ok(Protocol::PongMessage(pong)) => pong,
            // Anything else arriving on this socket is not something we asked
            // for; the probe advertises no contact info and so is not a peer.
            Ok(_) => return,
            Err(_) => {
                self.stats[index].invalid += 1;
                return;
            }
        };
        if pong.sanitize().is_err() || !pong.verify() {
            self.stats[index].invalid += 1;
            return;
        }
        // The token hash is the only field tying a pong to the ping which
        // produced it; `from` is remote-controlled and cannot be used as a key.
        let matched = self
            .outstanding
            .remove(pong.hash())
            .map(|(_index, sent_at)| sent_at.elapsed())
            .filter(|&rtt| rtt <= timeout);
        let stats = &mut self.stats[index];
        *stats.responders.entry(*pong.from()).or_default() += 1;
        match matched {
            Some(rtt) => {
                stats.received += 1;
                stats.rtts.push(rtt);
            }
            // Either the entry was already swept, or the hash matches nothing
            // we ever sent. Both mean "did not answer in time".
            None => stats.late += 1,
        }
    }

    fn expire(&mut self, timeout: Duration) {
        self.outstanding
            .retain(|_hash, (_index, sent_at)| sent_at.elapsed() <= timeout);
    }
}

/// Sends `config.count` rounds of pings to every target, one round every
/// `config.interval`, and returns per-target loss and latency.
///
/// A pong which arrives more than `config.timeout` after its ping is counted
/// as lost, not as slow.
pub fn run_ping_probe(
    targets: &[SocketAddr],
    identity_keypair: Option<Keypair>,
    config: &PingProbeConfig,
) -> io::Result<Vec<TargetStats>> {
    let targets: Vec<_> = targets.iter().copied().unique().collect();
    assert!(!targets.is_empty(), "need at least one target to probe");
    let keypair = identity_keypair.unwrap_or_else(Keypair::new);
    let socket = Arc::new(bind_to(config.bind_ip, 0)?);
    socket.set_read_timeout(Some(RECV_POLL_INTERVAL))?;
    info!(
        "pinging {} target(s) from {} as {}",
        targets.len(),
        socket.local_addr()?,
        keypair.pubkey(),
    );
    let state = Arc::new(Mutex::new(ProbeState::new(&targets)));
    let done_sending = Arc::new(AtomicBool::new(false));
    let sender = {
        let socket = Arc::clone(&socket);
        let state = Arc::clone(&state);
        let done_sending = Arc::clone(&done_sending);
        let (interval, count) = (config.interval, config.count);
        thread::Builder::new()
            .name("solGossipPing".to_string())
            .spawn(move || {
                send_pings(&socket, &state, &keypair, &targets, interval, count);
                done_sending.store(true, Ordering::Relaxed);
            })?
    };
    receive_pongs(&socket, &state, config, &done_sending)?;
    sender.join().expect("ping sender thread panicked");
    let stats = std::mem::take(&mut state.lock().unwrap().stats);
    Ok(stats)
}

fn send_pings(
    socket: &UdpSocket,
    state: &Mutex<ProbeState>,
    keypair: &Keypair,
    targets: &[SocketAddr],
    interval: Duration,
    count: Option<u64>,
) {
    let mut rng = rand::rng();
    for round in 0.. {
        if count.is_some_and(|count| round >= count) {
            break;
        }
        let round_start = Instant::now();
        for (index, addr) in targets.iter().enumerate() {
            let token: [u8; GOSSIP_PING_TOKEN_SIZE] = rng.random();
            let ping = Protocol::PingMessage(Ping::new(token, keypair));
            let ping = wincode::serialize(&ping).expect("Ping is always serializable");
            // Register before sending so that a pong cannot arrive before the
            // entry which it has to match against exists.
            {
                let mut state = state.lock().unwrap();
                state
                    .outstanding
                    .insert(hash_ping_token(&token), (index, Instant::now()));
                state.stats[index].sent += 1;
            }
            if let Err(err) = socket.send_to(&ping, addr) {
                warn!("failed to send ping to {addr}: {err}");
                let mut state = state.lock().unwrap();
                state.outstanding.remove(&hash_ping_token(&token));
                state.stats[index].sent -= 1;
                state.stats[index].send_errors += 1;
            }
        }
        if let Some(delay) = interval.checked_sub(round_start.elapsed()) {
            thread::sleep(delay);
        }
    }
}

fn receive_pongs(
    socket: &UdpSocket,
    state: &Mutex<ProbeState>,
    config: &PingProbeConfig,
    done_sending: &AtomicBool,
) -> io::Result<()> {
    let mut buffer = [0u8; PACKET_DATA_SIZE];
    let mut last_report = Instant::now();
    let mut drain_deadline = None;
    loop {
        match socket.recv_from(&mut buffer) {
            Ok((size, from)) => {
                state
                    .lock()
                    .unwrap()
                    .record_reply(&buffer[..size], from, config.timeout)
            }
            Err(err) if is_timeout(&err) => (),
            Err(err) => return Err(err),
        }
        state.lock().unwrap().expire(config.timeout);
        if last_report.elapsed() >= config.report_interval {
            last_report = Instant::now();
            for stats in &state.lock().unwrap().stats {
                info!(
                    "{}: sent {} received {} loss {:.2}% late {}",
                    stats.addr,
                    stats.sent,
                    stats.received,
                    stats.loss_percent(),
                    stats.late,
                );
            }
        }
        // Once the sender is done, keep draining for one timeout so that the
        // final round's pongs are not miscounted as lost.
        if done_sending.load(Ordering::Relaxed) {
            let deadline = *drain_deadline.get_or_insert(Instant::now() + config.timeout);
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }
}

fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}
