//! This module defines [`ArrayCache`], a fixed-size FIFO cache for keyed entries.
//! These structures provide mechanisms for caching values, looking them up, and evicting
//! the oldest entries when full.

use {
    crate::workers_cache::{WorkerInfo, WorkersCacheInterface},
    std::net::{Ipv4Addr, SocketAddr, SocketAddrV4},
};

pub struct ArrayCache<K, V> {
    entries: Box<[Option<V>]>,
    keys: Box<[K]>,
    order: Box<[u64]>,
    generation: u64,
}

impl<V> ArrayCache<SocketAddr, V> {
    pub fn new(capacity: usize) -> Self {
        Self::with_prototype(
            capacity,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
        )
    }
}

impl<K: Default + Clone + PartialEq, V> ArrayCache<K, V> {
    pub fn with_default_key(capacity: usize) -> Self {
        Self::with_prototype(capacity, K::default())
    }
}

impl<K, V> ArrayCache<K, V>
where
    K: Clone + PartialEq,
{
    pub fn with_prototype(capacity: usize, prototype: K) -> Self {
        Self {
            entries: (0..capacity)
                .map(|_| None)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            keys: std::iter::repeat_n(prototype, capacity)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            order: vec![0u64; capacity].into_boxed_slice(),
            generation: 0,
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Checks if a key is present in cache.
    fn contains(&self, key: &K) -> bool {
        self.entries
            .iter()
            .zip(self.keys.iter())
            .any(|(e, s)| e.is_some() && s == key)
    }

    /// Returns entry for a key without removing it.
    fn get(&mut self, key: &K) -> Option<&V> {
        self.entries
            .iter()
            .zip(self.keys.iter())
            .find(|(e, s)| e.is_some() && *s == key)
            .and_then(|(e, _s)| e.as_ref())
    }

    /// Inserts new key, entry, evicting oldest if full.
    fn push(&mut self, key: K, entry: V) -> Option<(K, V)> {
        self.generation = self.generation.checked_add(1).expect("generation overflow");
        let gen = self.generation;

        // replace existing one to avoid duplicates
        for i in 0..self.len() {
            if self.entries[i].is_some() && self.keys[i] == key {
                let evicted = self.entries[i].replace(entry).expect("entry must exist");
                self.order[i] = gen;

                return Some((key, evicted));
            }
        }

        // insert into first empty slot
        for i in 0..self.len() {
            if self.entries[i].is_none() {
                self.entries[i] = Some(entry);
                self.keys[i] = key;
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
            .replace(entry)
            .expect("entry must exist");
        let evicted_socket = self.keys[oldest_index].clone();

        self.keys[oldest_index] = key;
        self.order[oldest_index] = gen;

        Some((evicted_socket, evicted))
    }

    /// Removes and returns a entry for the given key, if present.
    fn pop(&mut self, key: &K) -> Option<V> {
        for (i, entry) in self.entries.iter_mut().enumerate() {
            if entry.is_some() && self.keys[i] == *key {
                self.order[i] = 0;

                return entry.take();
            }
        }

        None
    }

    /// Removes and returns oldest in cache, if present.
    fn pop_next(&mut self) -> Option<(K, V)> {
        let first = self.entries.iter().position(|e| e.is_some())?;
        let mut oldest_index = first;
        let mut oldest_gen = self.order[first];

        for i in (first..self.len()).skip(1) {
            if self.entries[i].is_some() && self.order[i] < oldest_gen {
                oldest_gen = self.order[i];
                oldest_index = i;
            }
        }

        let entry = self.entries[oldest_index].take().expect("entry must exist");
        let socket = self.keys[oldest_index].clone();
        self.order[oldest_index] = 0;

        Some((socket, entry))
    }
}

impl WorkersCacheInterface for ArrayCache<SocketAddr, WorkerInfo> {
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
        self.pop_next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn array_cache_contains() {
        let mut cache = ArrayCache::with_default_key(1);
        let addr1 = "1";

        assert!(!cache.contains(&addr1));

        cache.push(addr1, 1);
        assert!(cache.contains(&addr1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn array_cache_get() {
        let mut cache = ArrayCache::with_default_key(1);
        let addr1 = "1";

        assert!(cache.get(&addr1).is_none());

        cache.push(addr1, 1);
        assert!(cache.get(&addr1).is_some());
        // double check to make sure get doesnt evict entries
        assert!(cache.get(&addr1).is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn array_cache_push() {
        let mut cache = ArrayCache::with_default_key(4);

        let addr1 = "1";
        let addr2 = "2";
        let addr3 = "3";
        let addr4 = "4";
        let addr5 = "5";

        // add new ones within buf size
        assert!(cache.push(addr1, 1).is_none());
        assert!(cache.push(addr2, 2).is_none());
        assert!(cache.push(addr3, 3).is_none());
        assert!(cache.push(addr4, 4).is_none());

        assert!(cache.contains(&addr1));
        assert!(cache.contains(&addr2));
        assert!(cache.contains(&addr3));
        assert!(cache.contains(&addr4));

        // replace the existing entry for the same key
        let evicted = cache.push(addr1, 5);
        assert!(matches!(evicted, Some((key, _)) if key == addr1));
        assert!(cache.contains(&addr1));
        assert!(cache.contains(&addr2));
        assert!(cache.contains(&addr3));
        assert!(cache.contains(&addr4));

        // evict oldest
        let evicted = cache.push(addr5, 6);
        assert!(matches!(evicted, Some((key, _)) if key == addr2));

        assert!(cache.contains(&addr1));
        assert!(!cache.contains(&addr2));
        assert!(cache.contains(&addr3));
        assert!(cache.contains(&addr4));

        assert!(cache.contains(&addr5));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn array_cache_pop() {
        let mut cache = ArrayCache::with_default_key(2);

        let addr1 = "1";
        let addr2 = "2";

        assert!(cache.pop(&addr1).is_none());
        assert!(cache.pop(&addr2).is_none());

        cache.push(addr1, 1);
        cache.push(addr2, 2);

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
    async fn array_cache_pop_next() {
        let mut cache = ArrayCache::with_default_key(2);

        let addr1 = "1";
        let addr2 = "2";

        assert!(cache.pop_next().is_none());

        cache.push(addr1, 1);
        cache.push(addr2, 2);

        // evict addr1
        let evicted = cache.pop_next();
        assert!(matches!(evicted, Some((key, _)) if key == addr1));
        assert!(!cache.contains(&addr1));
        assert!(cache.contains(&addr2));

        // evict addr2
        let evicted = cache.pop_next();
        assert!(matches!(evicted, Some((key, _)) if key == addr2));
        assert!(!cache.contains(&addr2));
    }
}
