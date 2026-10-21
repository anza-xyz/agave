use {
    crate::{
        cluster_info::{GOSSIP_SLEEP_MILLIS, PULL_REQUEST_PERIOD},
        crds_gossip_pull::{CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS, MAX_NUM_FILTERS},
    },
    parking_lot::RwLock,
    std::{
        collections::{HashMap, VecDeque},
        net::SocketAddr,
        sync::atomic::{AtomicBool, Ordering},
        time::{Duration, Instant},
    },
};

/// Each pull round sends requests to at most MAX_NUM_FILTERS peers plus one
/// entrypoint, and pull rounds are at least PULL_REQUEST_PERIOD gossip rounds
/// apart, so no request is dropped before CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS.
const CAPACITY: usize = (MAX_NUM_FILTERS + 1)
    * (CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS / (GOSSIP_SLEEP_MILLIS * PULL_REQUEST_PERIOD as u64) + 1)
        as usize;

/// Requests are kept for twice the timeout so that late responses are counted
/// as expired rather than unsolicited.
const RETENTION: Duration = Duration::from_millis(2 * CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PullResponseStatus {
    Solicited,
    Expired,
    Unsolicited,
}

/// Addresses that pull requests were recently sent to.
#[derive(Default)]
pub(crate) struct PendingPullRequests {
    requests: RwLock<Requests>,
    // Entrypoints are pulled from without a ping check and may respond from
    // another address, so responses from any address are accepted until
    // there is a ping-verified pull peer.
    has_pull_peers: AtomicBool,
}

#[derive(Default)]
struct Requests {
    // Destination and send time of each pull request, oldest first.
    queue: VecDeque<(SocketAddr, Instant)>,
    // Send time of the latest pull request to each destination.
    latest: HashMap<SocketAddr, Instant>,
}

impl Requests {
    fn pop_front(&mut self) {
        if let Some((addr, sent)) = self.queue.pop_front()
            && self.latest.get(&addr) == Some(&sent)
        {
            self.latest.remove(&addr);
        }
    }
}

impl PendingPullRequests {
    pub(crate) fn record<I>(&self, addrs: I, now: Instant)
    where
        I: IntoIterator<Item = SocketAddr>,
    {
        let mut requests = self.requests.write();
        while requests
            .queue
            .front()
            .is_some_and(|(_, sent)| now.saturating_duration_since(*sent) > RETENTION)
        {
            requests.pop_front();
        }
        for addr in addrs {
            if requests.latest.insert(addr, now) != Some(now) {
                requests.queue.push_back((addr, now));
            }
        }
        while requests.queue.len() > CAPACITY {
            requests.pop_front();
        }
    }

    pub(crate) fn set_has_pull_peers(&self) {
        self.has_pull_peers.store(true, Ordering::Relaxed);
    }

    pub(crate) fn check(&self, addr: &SocketAddr, now: Instant) -> PullResponseStatus {
        if !self.has_pull_peers.load(Ordering::Relaxed) {
            return PullResponseStatus::Solicited;
        }
        // Responders drop pull requests whose wallclock is more than
        // CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS away from their own.
        let timeout = Duration::from_millis(CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS);
        match self.requests.read().latest.get(addr) {
            None => PullResponseStatus::Unsolicited,
            Some(sent) if now.saturating_duration_since(*sent) > timeout => {
                PullResponseStatus::Expired
            }
            Some(_) => PullResponseStatus::Solicited,
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::net::Ipv4Addr};

    fn make_addrs(round: usize) -> impl Iterator<Item = SocketAddr> {
        let ip = Ipv4Addr::from(0x0a00_0000 + u32::try_from(round).unwrap());
        (0..=MAX_NUM_FILTERS).map(move |port| SocketAddr::from((ip, u16::try_from(port).unwrap())))
    }

    #[test]
    fn test_pending_pull_requests_check() {
        let cache = PendingPullRequests::default();
        let timeout = Duration::from_millis(CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS);
        let now = Instant::now();
        let addr = SocketAddr::from(([10, 0, 0, 1], 8001));
        let other = SocketAddr::from(([10, 0, 0, 1], 8002));
        assert_eq!(cache.check(&addr, now), PullResponseStatus::Solicited);
        cache.set_has_pull_peers();
        assert_eq!(cache.check(&addr, now), PullResponseStatus::Unsolicited);
        cache.record([addr, addr], now);
        assert_eq!(cache.requests.read().queue.len(), 1);
        assert_eq!(cache.check(&addr, now), PullResponseStatus::Solicited);
        assert_eq!(cache.check(&other, now), PullResponseStatus::Unsolicited);
        assert_eq!(
            cache.check(&addr, now + timeout),
            PullResponseStatus::Solicited
        );
        let later = now + timeout + Duration::from_millis(1);
        assert_eq!(cache.check(&addr, later), PullResponseStatus::Expired);
        let later = now + RETENTION;
        cache.record([other], later);
        assert_eq!(cache.check(&addr, later), PullResponseStatus::Expired);
        let later = later + Duration::from_millis(1);
        cache.record([other], later);
        assert_eq!(cache.check(&addr, later), PullResponseStatus::Unsolicited);
        assert_eq!(cache.check(&other, later), PullResponseStatus::Solicited);
    }

    #[test]
    fn test_pending_pull_requests_capacity() {
        let pull_period = Duration::from_millis(GOSSIP_SLEEP_MILLIS * PULL_REQUEST_PERIOD as u64);
        let timeout = Duration::from_millis(CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS);
        let num_rounds = CAPACITY / (MAX_NUM_FILTERS + 1);
        let window = pull_period * u32::try_from(num_rounds).unwrap();
        assert!(window > timeout);
        assert!(window < RETENTION);
        let cache = PendingPullRequests::default();
        cache.set_has_pull_peers();
        let start = Instant::now();
        let sent = |round: usize| start + pull_period * u32::try_from(round).unwrap();
        for round in 0..4 * num_rounds {
            cache.record(make_addrs(round), sent(round));
            assert!(cache.requests.read().queue.len() <= CAPACITY);
            let oldest = (round + 1).saturating_sub(num_rounds);
            for addr in make_addrs(oldest) {
                assert_eq!(
                    cache.check(&addr, sent(oldest) + timeout),
                    PullResponseStatus::Solicited
                );
            }
        }
    }
}
