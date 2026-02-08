use super::*;

pub struct RingbufWorkersCache {
    cap: usize,
    order: VecDeque<SocketAddr>,
    map: HashMap<SocketAddr, WorkerInfo>,
}

impl RingbufWorkersCache {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            order: VecDeque::with_capacity(cap),
            map: HashMap::with_capacity(cap),
        }
    }

    /// drop socket address from order vec
    fn remove_from_order(&mut self, key: &SocketAddr) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
    }

    /// remove oldest from order vec + cleanup in map
    /// then returns the removed socketadr and worker info
    /// goes next in case of non existent map keys
    fn pop_oldest(&mut self) -> Option<(SocketAddr, WorkerInfo)> {
        while let Some(oldest) = self.order.pop_front() {
            if let Some(value) = self.map.remove(&oldest) {
                return Some((oldest, value));
            }
        }
        None
    }
}

impl WorkersCacheInterface for RingbufWorkersCache {
    fn contains(&self, key: &SocketAddr) -> bool {
        self.map.contains_key(key)
    }

    fn get(&mut self, key: &SocketAddr) -> Option<&WorkerInfo> {
        self.map.get(key)
    }

    fn push(&mut self, key: SocketAddr, value: WorkerInfo) -> Option<(SocketAddr, WorkerInfo)> {
        if let Some(prev) = self.map.remove(&key) {
            self.remove_from_order(&key);
            self.map.insert(key, value);
            self.order.push_back(key);

            return Some((key, prev));
        }

        let oldest = if self.map.len() == self.cap {
            self.pop_oldest()
        } else {
            None
        };

        self.map.insert(key, value);
        self.order.push_back(key);

        oldest
    }

    //
    fn pop(&mut self, key: &SocketAddr) -> Option<WorkerInfo> {
        let value = self.map.remove(key);
        if value.is_some() {
            self.remove_from_order(key);
        }
        value
    }

    fn pop_next(&mut self) -> Option<(SocketAddr, WorkerInfo)> {
        self.pop_oldest()
    }
}
