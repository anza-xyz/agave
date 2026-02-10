//! Simple cache implementation with fifo eviction.

use {
    super::*,
    std::net::{Ipv4Addr, SocketAddrV4},
};

pub struct SimpleCache {
    entries: Box<[Option<WorkerInfo>]>,
    sockets: Box<[SocketAddr]>,
    order: Box<[u64]>,
    generation: u64,
}

impl SimpleCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: (0..capacity)
                .map(|_| None)
                .collect::<Vec<Option<WorkerInfo>>>()
                .into_boxed_slice(),
            sockets: vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)); capacity]
                .into_boxed_slice(),
            order: vec![0u64; capacity].into_boxed_slice(),
            generation: 0,
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

impl WorkersCacheInterface for SimpleCache {
    /// Checks if a socket address is present in cache.
    fn contains(&self, socket: &SocketAddr) -> bool {
        self.entries
            .iter()
            .zip(self.sockets.iter())
            .any(|(e, s)| e.is_some() && s == socket)
    }

    /// Returns worker for a socket address without removing it.
    fn get(&mut self, socket: &SocketAddr) -> Option<&WorkerInfo> {
        self.entries
            .iter()
            .zip(self.sockets.iter())
            .find(|(e, s)| e.is_some() && *s == socket)
            .and_then(|(e, _s)| e.as_ref())
    }

    /// Inserts new worker, evicting oldest if full.
    fn push(&mut self, socket: SocketAddr, worker: WorkerInfo) -> Option<(SocketAddr, WorkerInfo)> {
        self.generation = self.generation.checked_add(1).expect("generation overflow");
        let gen = self.generation;

        // replace existing one to avoid duplicates
        for i in 0..self.len() {
            if self.entries[i].is_some() && self.sockets[i] == socket {
                let evicted = self.entries[i].replace(worker).expect("entry must exist");
                self.order[i] = gen;

                return Some((socket, evicted));
            }
        }

        // insert into first empty slot
        for i in 0..self.len() {
            if self.entries[i].is_none() {
                self.entries[i] = Some(worker);
                self.sockets[i] = socket;
                self.order[i] = gen;

                return None;
            }
        }

        // evict oldest
        let mut oldest_index = 0;
        let mut oldest_gen = self.generation;
        for i in 0..self.len() {
            if self.entries[i].is_some() && self.order[i] < oldest_gen {
                oldest_gen = self.order[i];
                oldest_index = i;
            }
        }

        let evicted = self.entries[oldest_index]
            .replace(worker)
            .expect("entry must exist");
        let evicted_socket = self.sockets[oldest_index];

        self.sockets[oldest_index] = socket;
        self.order[oldest_index] = gen;

        Some((evicted_socket, evicted))
    }

    /// Removes and returns worker for the given socket address, if present.
    fn pop(&mut self, socket: &SocketAddr) -> Option<WorkerInfo> {
        for (i, entry) in self.entries.iter_mut().enumerate() {
            if entry.is_some() && self.sockets[i] == *socket {
                self.order[i] = 0;
                self.sockets[i] = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));

                return entry.take();
            }
        }

        None
    }

    /// Removes and returns oldest worker in cache, if any.
    fn pop_next(&mut self) -> Option<(SocketAddr, WorkerInfo)> {
        let first = self.entries.iter().position(|e| e.is_some())?;
        let mut oldest_index = first;
        let mut oldest_gen = self.order[first];

        for i in (first + 1)..self.len() {
            if self.entries[i].is_some() && self.order[i] < oldest_gen {
                oldest_gen = self.order[i];
                oldest_index = i;
            }
        }

        let entry = self.entries[oldest_index].take().expect("entry must exist");
        let socket = self.sockets[oldest_index];

        self.order[oldest_index] = 0;
        self.sockets[oldest_index] = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));

        Some((socket, entry))
    }
}

#[cfg(test)]
mod tests {
    use {super::*, tokio::sync::mpsc, tokio_util::sync::CancellationToken};

    fn get_worker() -> WorkerInfo {
        let (sender, _) = mpsc::channel(1);
        let handle = tokio::spawn(async {});
        let cancel = CancellationToken::new();
        WorkerInfo::new(sender, handle, cancel)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn simple_cache_contains() {
        let mut cache = SimpleCache::new(1);
        let addr1 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8001));

        assert!(!cache.contains(&addr1));

        cache.push(addr1, get_worker());
        assert!(cache.contains(&addr1));

        assert!(cache.pop(&addr1).is_some());
        assert!(!cache.contains(&addr1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn simple_cache_get() {
        let mut cache = SimpleCache::new(1);
        let addr1 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8001));

        assert!(cache.get(&addr1).is_none());

        cache.push(addr1, get_worker());
        assert!(cache.get(&addr1).is_some());
        // double check to make sure get doesnt evict entries
        assert!(cache.get(&addr1).is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn simple_cache_push() {
        let mut cache = SimpleCache::new(4);

        let addr1 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8001));
        let addr2 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8002));
        let addr3 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8003));
        let addr4 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8004));
        let addr5 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8005));

        // add new ones within buf size
        assert!(cache.push(addr1, get_worker()).is_none());
        assert!(cache.push(addr2, get_worker()).is_none());
        assert!(cache.push(addr3, get_worker()).is_none());
        assert!(cache.push(addr4, get_worker()).is_none());

        assert!(cache.contains(&addr1));
        assert!(cache.contains(&addr2));
        assert!(cache.contains(&addr3));
        assert!(cache.contains(&addr4));

        // replace the existing entry for the same socket
        let evicted = cache.push(addr1, get_worker());
        assert!(matches!(evicted, Some((socket, _)) if socket == addr1));
        assert!(cache.contains(&addr1));
        assert!(cache.contains(&addr2));
        assert!(cache.contains(&addr3));
        assert!(cache.contains(&addr4));

        // evict oldest
        let evicted = cache.push(addr5, get_worker());
        assert!(matches!(evicted, Some((socket, _)) if socket == addr2));

        assert!(cache.contains(&addr1));
        assert!(!cache.contains(&addr2));
        assert!(cache.contains(&addr3));
        assert!(cache.contains(&addr4));

        assert!(cache.contains(&addr5));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn simple_cache_pop() {
        let mut cache = SimpleCache::new(2);

        let addr1 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8001));
        let addr2 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8002));

        assert!(cache.pop(&addr1).is_none());
        assert!(cache.pop(&addr2).is_none());

        cache.push(addr1, get_worker());
        cache.push(addr2, get_worker());

        // evict addr1
        assert!(cache.pop(&addr1).is_some());
        assert!(!cache.contains(&addr1));
        assert!(cache.contains(&addr2));

        // evict addr2
        assert!(cache.pop(&addr1).is_none());
        assert!(cache.pop(&addr2).is_some());
        assert!(!cache.contains(&addr2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn simple_cache_pop_next() {
        let mut cache = SimpleCache::new(2);

        let addr1 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8001));
        let addr2 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8002));

        assert!(cache.pop_next().is_none());

        cache.push(addr1, get_worker());
        cache.push(addr2, get_worker());

        // evict addr1
        let evicted = cache.pop_next();
        assert!(matches!(evicted, Some((socket, _)) if socket == addr1));
        assert!(!cache.contains(&addr1));
        assert!(cache.contains(&addr2));

        // evict addr2
        let evicted = cache.pop_next();
        assert!(matches!(evicted, Some((socket, _)) if socket == addr2));
        assert!(!cache.contains(&addr2));
    }
}
