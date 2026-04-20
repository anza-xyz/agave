use {super::*, ::lru::LruCache};

impl WorkersCacheInterface for LruCache<SocketAddr, WorkerInfo> {
    fn contains(&self, key: &SocketAddr) -> bool {
        self.contains(key)
    }

    fn get(&mut self, key: &SocketAddr) -> Option<&WorkerInfo> {
        self.get(key)
    }

    fn push(&mut self, key: SocketAddr, value: WorkerInfo) -> Option<(SocketAddr, WorkerInfo)> {
        self.push(key, value)
    }

    fn pop(&mut self, key: &SocketAddr) -> Option<WorkerInfo> {
        self.pop(key)
    }

    fn pop_next(&mut self) -> Option<(SocketAddr, WorkerInfo)> {
        self.pop_lru()
    }
}
