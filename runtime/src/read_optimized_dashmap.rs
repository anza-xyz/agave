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

// Wrapper around dashmap that stores (K, Arc<V>) to minimize shard contention.
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

    /// Alternative to entry(k).or_insert_with(default) that returns an Arc<V> instead of returning a
    /// guard that holds the underlying shard's write lock.
    ///
    /// # Safety
    ///
    /// - Care must be taken when mutating the returned value. If modifications are applied while
    ///   another thread concurrently deletes the key, the changes may be lost.
    pub unsafe fn get_or_insert_with(&self, k: &K, default: impl FnOnce() -> V) -> ROValue<V> {
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
    ///
    /// # Safety
    ///
    /// - Care must be taken when mutating the returned value. If modifications are applied while
    ///   another thread concurrently deletes the key, the changes may be lost.
    pub unsafe fn get(&self, k: &K) -> Option<ROValue<V>> {
        self.inner.get(k).map(|v| ROValue::clone(&v))
    }

    pub fn iter(&self) -> impl Iterator<Item = DashmapIteratorItem<K, V, S>> {
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
    pub fn remove_if_not_accessed_and<P: Fn(&ROValue<V>) -> bool>(
        &self,
        k: &K,
        pred: P,
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
#[derive(Debug, Default)]
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
        let v1 = unsafe { map.get_or_insert_with(&1, || 10) };
        assert_eq!(*v1, 10);
        assert!(v1.shared());
        let v2 = unsafe { map.get(&1).unwrap() };
        assert_eq!(*v2, 10);
        assert!(v2.shared());
        let v3 = unsafe { map.get(&2) };
        assert!(v3.is_none());
    }

    #[test]
    fn test_remove_if_not_accessed() {
        let map = ReadOptimizedDashMap::new(DashMap::with_hasher_and_shard_amount(
            RandomState::default(),
            4,
        ));
        let v1 = unsafe { map.get_or_insert_with(&1, || 10) };
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
}
