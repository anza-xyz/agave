#![allow(dead_code)]

#[cfg(feature = "shuttle-test")]
use shuttle::sync::Arc;
#[cfg(not(feature = "shuttle-test"))]
use std::sync::Arc;
use {
    dashmap::{
        mapref::{entry::Entry, multiple::RefMulti},
        DashMap,
    },
    std::{hash::BuildHasher, ops::Deref},
};

type DashmapIteratorItem<'a, K, V, S> = RefMulti<'a, K, ROValue<V>, S>;

// Wrapper around dashmap that stores `Arc`s of values to minimize shard contention.
#[derive(Debug)]
pub struct ReadOptimizedDashMap<K, V, S>
where
    K: Clone + Eq + std::hash::Hash,
    S: Clone + BuildHasher,
{
    inner: DashMap<K, ROValue<V>, S>,
}

impl<K, V, S> ReadOptimizedDashMap<K, V, S>
where
    K: Clone + Eq + std::hash::Hash,
    S: Clone + BuildHasher,
{
    pub fn new(inner: DashMap<K, ROValue<V>, S>) -> Self {
        Self { inner }
    }

    /// Alternative to entry(k).or_insert_with(default) that returns a cloned `Arc` instead of
    /// returning a guard that holds the underlying shard's write lock.
    pub fn get_or_insert_with(&self, k: &K, default: impl FnOnce() -> V) -> ROValue<V> {
        match self.inner.get(k) {
            Some(v) => ROValue::clone(&*v),
            None => ROValue::clone(
                self.inner
                    .entry(k.clone())
                    .or_insert_with(|| ROValue::new(default()))
                    .value(),
            ),
        }
    }

    /// Returns an Arc clone of the value corresponding to the key.
    pub fn get(&self, k: &K) -> Option<ROValue<V>> {
        self.inner.get(k).map(|v| ROValue::clone(&v))
    }

    pub fn iter(&self) -> impl Iterator<Item = DashmapIteratorItem<'_, K, V, S>> {
        self.inner.iter()
    }

    /// Removes the entry if it exists and is not being accessed by any other threads.
    ///
    /// Returns Ok(Some(value)) if the entry was removed, Ok(None) if the entry did not exist, and
    /// Err(()) if the entry exists but is being accessed by another thread.
    pub fn remove_if_not_accessed(&self, k: &K) -> Result<Option<ROValue<V>>, ()> {
        self.remove_if_not_accessed_and(k, |_| true)
    }

    /// Removes the entry if it exists, it is not being accessed by any other
    /// threads and the predicate returns true.
    ///
    /// Returns Ok(Some(value)) if the entry was removed, Ok(None) if the entry did not exist, and
    /// Err(()) if the entry exists but is being accessed by another thread.
    pub fn remove_if_not_accessed_and(
        &self,
        k: &K,
        pred: impl Fn(&ROValue<V>) -> bool,
    ) -> Result<Option<ROValue<V>>, ()> {
        let entry = self.inner.entry(k.clone());
        if let Entry::Occupied(e) = entry {
            let v = e.get();
            // the entry guard holds a write lock, so checking for strong_count is safe
            if pred(v) && !v.shared() {
                return Ok(Some(e.remove()));
            } else {
                return Err(());
            }
        }
        Ok(None)
    }

    /// Retains only the elements specified by the predicate or that are being
    /// accessed by other threads.
    pub fn retain_if_accessed_or(&self, mut f: impl FnMut(&K, &mut ROValue<V>) -> bool) {
        self.inner.retain(|k, v| v.shared() || f(k, v))
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// # Safety
    ///
    /// - If called concurrently with other methods that mutate values that are
    ///   not retained, the modifications may be lost.
    pub unsafe fn retain(&self, f: impl FnMut(&K, &mut ROValue<V>) -> bool) {
        self.inner.retain(f)
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn clear(&self) {
        self.inner.clear();
    }
}

/// A value held inside a ReadOptimizedDashMap.
///
/// This type is a wrapper around Arc that allows checking whether there are
/// other strong references to the inner value.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct ROValue<V> {
    inner: Arc<V>,
}

impl<V> Clone for ROValue<V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<V> ROValue<V> {
    fn new(v: V) -> Self {
        Self { inner: Arc::new(v) }
    }

    /// Returns true if there are other strong references to the inner value.
    pub fn shared(&self) -> bool {
        Arc::strong_count(&self.inner) > 1
    }

    /// Returns a reference to the inner Arc<V>.
    pub fn inner(&self) -> &Arc<V> {
        &self.inner
    }
}

impl<V> Deref for ROValue<V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::hash::RandomState};

    #[test]
    fn test_get() {
        let map = ReadOptimizedDashMap::new(DashMap::with_hasher_and_shard_amount(
            RandomState::default(),
            4,
        ));
        let v1 = map.get_or_insert_with(&1, || 10);
        assert_eq!(*v1, 10);
        assert!(v1.shared());
        let v2 = map.get(&1).unwrap();
        assert_eq!(*v2, 10);
        assert!(v2.shared());
        let v3 = map.get(&2);
        assert!(v3.is_none());
    }

    #[test]
    fn test_remove_if_not_accessed() {
        let map = ReadOptimizedDashMap::new(DashMap::with_hasher_and_shard_amount(
            RandomState::default(),
            4,
        ));
        let v1 = map.get_or_insert_with(&1, || 10);
        assert_eq!(*v1, 10);
        // cannot remove while v1 is held
        assert!(map.remove_if_not_accessed(&1).is_err());
        drop(v1);
        // pred returns false
        let removed = map.remove_if_not_accessed_and(&1, |_| false);
        assert!(removed.is_err());
        // can remove now that v1 is dropped
        let removed = map.remove_if_not_accessed(&1).unwrap();
        assert!(removed.is_some());
        assert_eq!(*removed.unwrap(), 10);
        // cannot remove non-existent key
        let removed = map.remove_if_not_accessed(&1).unwrap();
        assert!(removed.is_none());
    }

    #[test]
    fn test_retain_if_accessed_or() {
        let map = ReadOptimizedDashMap::new(DashMap::with_hasher_and_shard_amount(
            RandomState::default(),
            4,
        ));

        let v1 = map.get_or_insert_with(&1, || 10);
        let v2 = map.get_or_insert_with(&2, || 20);
        drop(v2);
        let v3 = map.get_or_insert_with(&3, || 30);
        assert_eq!(map.inner.len(), 3);
        map.retain_if_accessed_or(|_k, v| **v >= 30);
        assert_eq!(map.inner.len(), 2);
        drop(v1);
        drop(v3);
        map.retain_if_accessed_or(|_k, v| **v >= 30);
        assert_eq!(map.inner.len(), 1);
    }
}

#[cfg(all(test, feature = "shuttle-test"))]
mod shuttle_tests {
    use {
        super::*,
        shuttle::{sync::atomic::AtomicU64, thread},
        std::{hash::RandomState, sync::atomic::Ordering},
    };

    const INSERT_RETAIN_RANDOM_ITERATIONS: usize = 200000;
    const INSERT_RETAIN_DFS_ITERATIONS: Option<usize> = None;

    const CONCURRENT_INSERTS_RANDOM_ITERATIONS: usize = 200000;
    const CONCURRENT_INSERTS_DFS_ITERATIONS: Option<usize> = None;

    fn do_test_shuttle_concurrent_inserts() {
        let map = Arc::new(ReadOptimizedDashMap::new(
            DashMap::with_hasher_and_shard_amount(RandomState::default(), 4),
        ));
        let handles = (0..2)
            .map(|_| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    map.get_or_insert_with(&0, || AtomicU64::new(30))
                        .fetch_add(10, Ordering::Relaxed);
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        let v = map.get(&0).unwrap();
        assert_eq!(v.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn test_shuttle_concurrent_inserts_dfs() {
        shuttle::check_dfs(
            do_test_shuttle_concurrent_inserts,
            CONCURRENT_INSERTS_DFS_ITERATIONS,
        );
    }

    #[test]
    fn test_shuttle_concurrent_inserts_random() {
        shuttle::check_random(
            do_test_shuttle_concurrent_inserts,
            CONCURRENT_INSERTS_RANDOM_ITERATIONS,
        );
    }

    fn do_test_shuttle_insert_retain() {
        let map = Arc::new(ReadOptimizedDashMap::new(
            DashMap::with_hasher_and_shard_amount(RandomState::default(), 4),
        ));
        map.get_or_insert_with(&1, || 10);
        let retain_th = thread::spawn({
            let map = Arc::clone(&map);
            move || {
                map.retain_if_accessed_or(|_k, v| **v >= 20);
            }
        });
        let insert_th = thread::spawn({
            let map = Arc::clone(&map);
            move || {
                map.get_or_insert_with(&2, || 20);
            }
        });
        retain_th.join().unwrap();
        insert_th.join().unwrap();
        assert_eq!(map.get(&1), None);
        let v = map.get(&2).unwrap();
        assert_eq!(*v, 20);
    }

    #[test]
    fn test_shuttle_insert_retain_dfs() {
        shuttle::check_dfs(do_test_shuttle_insert_retain, INSERT_RETAIN_DFS_ITERATIONS);
    }

    #[test]
    fn test_shuttle_insert_retain_random() {
        shuttle::check_random(
            do_test_shuttle_insert_retain,
            INSERT_RETAIN_RANDOM_ITERATIONS,
        );
    }
}
