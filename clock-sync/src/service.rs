//! The clock-sync service thread: drives the Welch-Lynch state machine
//! against real time and the QUIC datagram transport. Per round: broadcast
//! our pulse at `next`, drain inbound pulses, close at `next + W` and apply
//! the correction to the shared [`SyncedClock`].

use {
    crate::{
        ABSORPTION_HALF_WIDTH, ACCEPTANCE_WINDOW, PULSE_PERIOD,
        clock::{LocalNs, SyncedClock},
        delay::DelayTracker,
        protocol::{self, Message},
        stats::RoundStats,
        welch_lynch::{Config, PulseReject, WelchLynch},
    },
    agave_votor_transport::endpoint::Datagram,
    arc_swap::ArcSwap,
    bytes::Bytes,
    crossbeam_channel::{Receiver, RecvTimeoutError},
    log::{info, warn},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::Duration,
    },
    tokio::sync::mpsc,
};

/// Upper bound on a single ingress wait so the exit flag is honored promptly.
const MAX_WAIT: Duration = Duration::from_millis(100);

/// Shared view of the current-epoch staked set, published by the peer-list
/// updater in `core` and read at every round close.
pub type SharedStakes = Arc<ArcSwap<HashMap<Pubkey, u64>>>;

/// The protocol's timing constants. Defaults are the production values;
/// tests shrink them to run many rounds quickly.
#[derive(Debug, Clone, Copy)]
pub struct ServiceTiming {
    /// T: pulse period.
    pub period: Duration,
    /// W: acceptance window; must be strictly less than `period`.
    pub window: Duration,
    /// S: absorption cluster half-width.
    pub absorption_half_width: Duration,
}

impl Default for ServiceTiming {
    fn default() -> Self {
        Self {
            period: PULSE_PERIOD,
            window: ACCEPTANCE_WINDOW,
            absorption_half_width: ABSORPTION_HALF_WIDTH,
        }
    }
}

pub struct ClockSyncService {
    thread: JoinHandle<()>,
}

impl ClockSyncService {
    pub fn new(
        identity: Pubkey,
        timing: ServiceTiming,
        clock: Arc<SyncedClock>,
        egress: mpsc::Sender<Bytes>,
        ingress: Receiver<Datagram>,
        stakes: SharedStakes,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread = Builder::new()
            .name("solClockSync".to_string())
            .spawn(move || run(identity, timing, &clock, &egress, &ingress, &stakes, &exit))
            .expect("spawn solClockSync");
        Self { thread }
    }

    pub fn join(self) -> std::thread::Result<()> {
        self.thread.join()
    }
}

fn run(
    identity: Pubkey,
    timing: ServiceTiming,
    clock: &SyncedClock,
    egress: &mpsc::Sender<Bytes>,
    ingress: &Receiver<Datagram>,
    stakes: &SharedStakes,
    exit: &AtomicBool,
) {
    let period_ns = timing.period.as_nanos() as i64;
    let config = Config {
        period_ns,
        window_ns: timing.window.as_nanos() as i64,
        absorption_half_width_ns: timing.absorption_half_width.as_nanos() as i64,
    };

    // Bootstrap from the wall clock: the pulse at unix time k*T belongs to
    // round k, consistent across validators whose system clocks agree to
    // within ~W. Phase 2's coarse loop replaces this.
    let start_round = clock
        .local_now()
        .ns()
        .div_euclid(period_ns)
        .saturating_add(1);
    let first_next = LocalNs::from_ns(start_round.saturating_mul(period_ns));
    let mut machine = WelchLynch::new(config, identity, start_round as u64, first_next);
    info!(
        "clock-sync started: round {start_round}, T {:?}, W {:?}",
        timing.period, timing.window
    );

    let mut delays = DelayTracker::new();
    let mut stats = RoundStats::default();
    let mut pulse_sent = false;

    while !exit.load(Ordering::Relaxed) {
        let now = clock.local_now();
        let due = if pulse_sent {
            machine.round_close_at()
        } else {
            machine.pulse_due_at()
        };

        if now >= due {
            if pulse_sent {
                close_round(&mut machine, clock, stakes, &delays, &mut stats);
                let stakes = stakes.load();
                delays.retain(|peer| stakes.contains_key(peer));
                pulse_sent = false;
            } else {
                send_pulse(&machine, clock, egress, &mut stats);
                machine.record_own_pulse();
                pulse_sent = true;
            }
            continue;
        }

        let wait = Duration::from_nanos(due.saturating_sub(now) as u64).min(MAX_WAIT);
        match ingress.recv_timeout(wait) {
            Ok(datagram) => handle_datagram(datagram, &mut machine, clock, &mut delays, &mut stats),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                warn!("clock-sync ingress disconnected; exiting");
                break;
            }
        }
    }
}

fn handle_datagram(
    datagram: Datagram,
    machine: &mut WelchLynch,
    clock: &SyncedClock,
    delays: &mut DelayTracker,
    stats: &mut RoundStats,
) {
    let Datagram {
        peer_pubkey,
        path_rtt,
        message,
        ..
    } = datagram;
    // Arrival is stamped at dequeue, not at the socket read; the ingress
    // channel hop lands in the delay-uncertainty term.
    let arrival = clock.local_now();
    let Some(Message::Pulse { round, lateness_ns }) = protocol::decode(&message) else {
        stats.decode_errors = stats.decode_errors.saturating_add(1);
        return;
    };
    delays.observe(peer_pubkey, path_rtt, arrival);
    let delay_ns = delays
        .delay_ns(&peer_pubkey, arrival)
        .expect("peer was observed just above");
    match machine.on_pulse(peer_pubkey, round, arrival, delay_ns, lateness_ns) {
        Ok(()) => {}
        Err(PulseReject::StaleRound) => stats.stale_round = stats.stale_round.saturating_add(1),
        Err(PulseReject::FarFutureRound) => {
            stats.far_future_round = stats.far_future_round.saturating_add(1)
        }
        Err(PulseReject::DuplicatePeer) => {
            stats.duplicate_peer = stats.duplicate_peer.saturating_add(1)
        }
    }
}

fn send_pulse(
    machine: &WelchLynch,
    clock: &SyncedClock,
    egress: &mpsc::Sender<Bytes>,
    stats: &mut RoundStats,
) {
    let lateness_ns = clock
        .local_now()
        .saturating_sub(machine.pulse_due_at())
        .max(0);
    stats.send_lateness_ns = lateness_ns;
    let pulse = protocol::encode_pulse(machine.round(), lateness_ns);
    if egress.try_send(pulse).is_err() {
        stats.egress_full = stats.egress_full.saturating_add(1);
    }
}

fn close_round(
    machine: &mut WelchLynch,
    clock: &SyncedClock,
    stakes: &SharedStakes,
    delays: &DelayTracker,
    stats: &mut RoundStats,
) {
    let round = machine.round();
    let stakes = stakes.load();
    let n_peers = stakes.len();
    let outcome = machine.close_round(&stakes);
    clock.set_offset_ns(machine.cumulative_offset_ns());
    stats.report(
        round,
        &outcome,
        machine.cumulative_offset_ns(),
        clock.offset_vs_system_ns(),
        n_peers,
        delays,
        clock.local_now(),
    );
}
