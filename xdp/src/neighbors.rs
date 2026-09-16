use {
    crate::netlink::{NetlinkSocket, netlink_use_neighbor},
    crossbeam_channel::{RecvTimeoutError, Sender},
    log::{debug, warn},
    std::{
        collections::{HashMap, hash_map::Entry},
        io,
        net::Ipv4Addr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    },
};

const NEIGHBOR_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Intervals between neighbor refreshes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NeighborIntervals {
    /// How often to request the kernel to touch a resolved neighbor so it doesn't go stale.
    pub use_interval: Duration,
    /// How often to try to resolve an unresolved neighbor.
    pub miss_interval: Duration,
}

impl NeighborIntervals {
    fn min_interval(self, is_resolved: bool) -> Duration {
        if is_resolved {
            self.use_interval
        } else {
            self.miss_interval
        }
    }
}

#[derive(Clone)]
/// Observes neighbors and keeps them fresh in the neigh table.
///
/// Linux expires neighbors from the neigh table after a certain idle time. When using regular
/// sockets, the kernel automatically refreshes neighbors when sending packets to them. When using
/// XDP, we need to explicitly keep them alive.
pub(crate) struct NeighborsObserver {
    sender: Sender<NeighborEvent>,
    intervals: NeighborIntervals,
    neighbors: HashMap<NeighborKey, NeighborState>,
    next_sweep_at: Option<Instant>,
}

impl NeighborsObserver {
    fn new(sender: Sender<NeighborEvent>, intervals: NeighborIntervals) -> Self {
        Self {
            sender,
            intervals,
            neighbors: HashMap::new(),
            next_sweep_at: None,
        }
    }

    pub(crate) fn observe(&mut self, if_index: u32, ip: Ipv4Addr, is_resolved: bool) {
        self.observe_at(if_index, ip, is_resolved, Instant::now());
    }

    fn observe_at(&mut self, if_index: u32, ip: Ipv4Addr, is_resolved: bool, now: Instant) {
        let key = NeighborKey { if_index, ip };

        sweep(&mut self.next_sweep_at, &mut self.neighbors, now);

        match self.neighbors.entry(key) {
            Entry::Vacant(entry) => {
                if self
                    .sender
                    .try_send(NeighborEvent { key, is_resolved })
                    .is_ok()
                {
                    entry.insert(NeighborState {
                        last_touched_at: now,
                    });
                }
            }
            Entry::Occupied(mut entry) => {
                let state = entry.get_mut();
                if now.duration_since(state.last_touched_at)
                    < self.intervals.min_interval(is_resolved)
                {
                    return;
                }

                if self
                    .sender
                    .try_send(NeighborEvent { key, is_resolved })
                    .is_ok()
                {
                    state.last_touched_at = now;
                }
            }
        };
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct NeighborKey {
    if_index: u32,
    ip: Ipv4Addr,
}

#[derive(Clone, Copy, Debug)]
struct NeighborState {
    last_touched_at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct NeighborEvent {
    key: NeighborKey,
    is_resolved: bool,
}

/// Submits USE requests for neighbors observed by the `NeighborsObserver`.
///
/// This is a separate thread since netlink requires CAP_NET_ADMIN and we don't want to require that
/// capability for all the XDP senders.
pub(crate) struct NeighborsRefresher {
    socket: NetlinkSocket,
    intervals: NeighborIntervals,
    neighbors: HashMap<NeighborKey, NeighborState>,
    next_sweep_at: Option<Instant>,
}

impl NeighborsRefresher {
    pub(crate) fn start<F: FnOnce() + Send + Sync + 'static>(
        exit: Arc<AtomicBool>,
        intervals: NeighborIntervals,
        on_thread_start: F,
    ) -> io::Result<(thread::JoinHandle<()>, NeighborsObserver)> {
        const NEIGHBOR_MONITOR_RECV_TIMEOUT: Duration = Duration::from_millis(100);
        const NEIGHBOR_REQUEST_CHANNEL_CAP: usize = 8192;

        let (sender, receiver) = crossbeam_channel::bounded(NEIGHBOR_REQUEST_CHANNEL_CAP);
        let handle = thread::Builder::new()
            .name("solNeighMon".to_owned())
            .spawn(move || {
                on_thread_start();

                let mut monitor = Self::new(intervals);
                while !exit.load(Ordering::Relaxed) {
                    match receiver.recv_timeout(NEIGHBOR_MONITOR_RECV_TIMEOUT) {
                        Ok(event) => monitor.observe_event(event, Instant::now()),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })?;

        Ok((handle, NeighborsObserver::new(sender, intervals)))
    }

    fn new(intervals: NeighborIntervals) -> Self {
        Self {
            socket: NetlinkSocket::open().expect("failed to open netlink socket"),
            intervals,
            neighbors: HashMap::new(),
            next_sweep_at: None,
        }
    }

    fn observe_event(&mut self, event: NeighborEvent, now: Instant) {
        self.observe_at(event.key, event.is_resolved, now)
    }

    fn observe_at(&mut self, key: NeighborKey, is_resolved: bool, now: Instant) {
        sweep(&mut self.next_sweep_at, &mut self.neighbors, now);

        let entry = self.neighbors.entry(key);
        if let Entry::Occupied(entry) = &entry
            && now.duration_since(entry.get().last_touched_at)
                < self.intervals.min_interval(is_resolved)
        {
            return;
        }

        match netlink_use_neighbor(&self.socket, key.if_index, key.ip) {
            Ok(()) => {
                entry.insert_entry(NeighborState {
                    last_touched_at: now,
                });
                debug!("refreshed neighbor {} on if{}", key.ip, key.if_index);
            }
            Err(err) => {
                warn!(
                    "failed to request neighbor use {} on if{}: {}",
                    key.ip, key.if_index, err
                );
                self.socket = NetlinkSocket::open().expect("failed to reopen netlink socket");
            }
        }
    }
}

fn sweep(
    next_sweep_at: &mut Option<Instant>,
    neighbors: &mut HashMap<NeighborKey, NeighborState>,
    now: Instant,
) {
    let next_sweep_at =
        next_sweep_at.get_or_insert_with(|| now.checked_add(NEIGHBOR_IDLE_TIMEOUT).unwrap());
    if now >= *next_sweep_at {
        neighbors
            .retain(|_, state| now.duration_since(state.last_touched_at) < NEIGHBOR_IDLE_TIMEOUT);
        *next_sweep_at = now.checked_add(NEIGHBOR_IDLE_TIMEOUT).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::{Receiver, TryRecvError, bounded},
    };

    fn test_key() -> NeighborKey {
        NeighborKey {
            if_index: 7,
            ip: Ipv4Addr::new(10, 0, 0, 7),
        }
    }

    fn recv_event(receiver: &Receiver<NeighborEvent>) -> NeighborEvent {
        receiver.try_recv().expect("expected neighbor event")
    }

    #[test]
    fn observe_dedupes_within_resolved_cooldown() {
        let (sender, receiver) = bounded(8);
        let intervals = NeighborIntervals {
            use_interval: Duration::from_secs(30),
            miss_interval: Duration::from_secs(1),
        };
        let mut neighbors = NeighborsObserver::new(sender, intervals);
        let key = test_key();
        let now = Instant::now();

        neighbors.observe_at(key.if_index, key.ip, true, now);
        assert_eq!(recv_event(&receiver).key, key);

        neighbors.observe_at(key.if_index, key.ip, true, now + Duration::from_secs(1));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

        neighbors.observe_at(key.if_index, key.ip, true, now + intervals.use_interval);
        assert_eq!(recv_event(&receiver).key, key);
    }

    #[test]
    fn observe_uses_shorter_cooldown_for_unresolved_neighbors() {
        let (sender, receiver) = bounded(8);
        let intervals = NeighborIntervals {
            use_interval: Duration::from_secs(30),
            miss_interval: Duration::from_secs(1),
        };
        let mut neighbors = NeighborsObserver::new(sender, intervals);
        let key = test_key();
        let now = Instant::now();

        neighbors.observe_at(key.if_index, key.ip, false, now);
        assert_eq!(recv_event(&receiver).key, key);

        neighbors.observe_at(
            key.if_index,
            key.ip,
            false,
            now + Duration::from_millis(999),
        );
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

        neighbors.observe_at(key.if_index, key.ip, false, now + intervals.miss_interval);
        assert_eq!(recv_event(&receiver).key, key);
    }
}
