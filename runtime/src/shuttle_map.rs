#![allow(dead_code)]

#[cfg(feature = "shuttle-test")]
use shuttle::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
#[cfg(not(feature = "shuttle-test"))]
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::{
    collections::HashMap,
    hash::{BuildHasher, Hash, RandomState},
    ops::{Deref, DerefMut},
};

#[derive(Debug)]
pub struct ShuttleMap<K, V, S = RandomState>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    shards: Vec<RwLock<HashMap<K, V, S>>>,
    hasher: S,
}

impl<K, V, S> Default for ShuttleMap<K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher + Default + Clone,
{
    fn default() -> Self {
        Self {
            shards: (0..4)
                .map(|_| RwLock::new(HashMap::with_hasher(S::default())))
                .collect(),
            hasher: S::default(),
        }
    }
}

impl<K, V> ShuttleMap<K, V, RandomState>
where
    K: Eq + Hash + Clone,
{
    pub fn new(shard_count: usize) -> Self {
        Self::with_hasher_and_shard_amount(RandomState::default(), shard_count)
    }
}

impl<K, V, S> ShuttleMap<K, V, S>
where
    K: Eq + Hash + Clone,
    S: BuildHasher + Clone,
{
    pub fn with_hasher_and_shard_amount(hasher: S, shard_count: usize) -> Self {
        let shard_count = shard_count.max(2).next_power_of_two();
        let mut shards = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            shards.push(RwLock::new(HashMap::with_hasher(hasher.clone())));
        }
        Self { shards, hasher }
    }

    fn shard_for<Q>(&self, k: &Q) -> usize
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash,
    {
        self.hasher.hash_one(k) as usize % self.shards.len()
    }

    pub fn get(&self, k: &K) -> Option<ReadGuard<'_, K, V, S>> {
        let idx = self.shard_for(k);
        let guard = self.shards[idx].read().unwrap();
        if guard.contains_key(k) {
            Some(ReadGuard {
                guard,
                key: k.clone(),
            })
        } else {
            None
        }
    }

    pub fn insert(&self, k: K, v: V) -> Option<V> {
        let idx = self.shard_for(&k);
        let mut guard = self.shards[idx].write().unwrap();
        guard.insert(k, v)
    }

    pub fn remove(&self, k: &K) -> Option<(K, V)> {
        let idx = self.shard_for(k);
        let mut guard = self.shards[idx].write().unwrap();
        guard.remove(k).map(|v| (k.clone(), v))
    }

    pub fn clear(&self) {
        for shard in &self.shards {
            let mut guard = shard.write().unwrap();
            guard.clear();
        }
    }

    pub fn entry(&self, k: K) -> Entry<'_, K, V, S> {
        let idx = self.shard_for(&k);
        let guard = self.shards[idx].write().unwrap();
        if guard.contains_key(&k) {
            Entry::Occupied(OccupiedEntry { guard, key: k })
        } else {
            Entry::Vacant(VacantEntry { guard, key: k })
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = ReadGuard<'_, K, V, S>>
    where
        K: Clone,
    {
        let mut out = Vec::new();
        for shard in &self.shards {
            let keys: Vec<K> = {
                let guard = shard.read().unwrap();
                guard.keys().cloned().collect()
            };

            for key in keys {
                let guard = shard.read().unwrap();
                if guard.contains_key(&key) {
                    out.push(ReadGuard { guard, key });
                }
            }
        }
        out.into_iter()
    }

    fn for_each(&self, mut f: impl FnMut(&K, &V)) {
        for shard in &self.shards {
            let guard = shard.read().unwrap();
            for (k, v) in guard.iter() {
                f(k, v);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        for shard in &self.shards {
            let guard = shard.read().unwrap();
            if !guard.is_empty() {
                return false;
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        let mut len = 0;
        self.for_each(|_k, _v| len += 1);
        len
    }

    pub fn retain<F>(&self, mut f: F)
    where
        F: FnMut(&K, &mut V) -> bool,
    {
        for shard in &self.shards {
            let mut guard = shard.write().unwrap();
            guard.retain(|k, v| f(k, v));
        }
    }
}

impl<K, V, S> FromIterator<(K, V)> for ShuttleMap<K, V, S>
where
    K: Eq + Hash + Clone,
    S: BuildHasher + Default + Clone,
{
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let map = Self::with_hasher_and_shard_amount(S::default(), 4);
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

pub enum Entry<'a, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    Occupied(OccupiedEntry<'a, K, V, S>),
    Vacant(VacantEntry<'a, K, V, S>),
}

impl<'a, K, V, S> Entry<'a, K, V, S>
where
    K: Eq + Hash + Clone,
    S: BuildHasher,
{
    pub fn or_insert_with(self, f: impl FnOnce() -> V) -> WriteGuard<'a, K, V, S> {
        match self {
            Entry::Occupied(o) => WriteGuard {
                guard: o.guard,
                key: o.key,
            },
            Entry::Vacant(mut v) => {
                v.guard.insert(v.key.clone(), f());
                WriteGuard {
                    guard: v.guard,
                    key: v.key,
                }
            }
        }
    }
}

pub struct OccupiedEntry<'a, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    guard: RwLockWriteGuard<'a, HashMap<K, V, S>>,
    key: K,
}

impl<K, V, S> OccupiedEntry<'_, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    pub fn get(&self) -> &V {
        self.guard.get(&self.key).unwrap()
    }
    pub fn get_mut(&mut self) -> &mut V {
        self.guard.get_mut(&self.key).unwrap()
    }
    pub fn remove(mut self) -> V {
        self.guard.remove(&self.key).unwrap()
    }
}

pub struct VacantEntry<'a, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    guard: RwLockWriteGuard<'a, HashMap<K, V, S>>,
    key: K,
}

pub struct ReadGuard<'a, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    guard: RwLockReadGuard<'a, HashMap<K, V, S>>,
    key: K,
}

impl<K, V, S> ReadGuard<'_, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn value(&self) -> &V {
        self.guard.get(&self.key).unwrap()
    }

    pub fn pair(&self) -> (&K, &V) {
        (&self.key, self.guard.get(&self.key).unwrap())
    }
}

impl<K, V, S> std::ops::Deref for ReadGuard<'_, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    type Target = V;
    fn deref(&self) -> &Self::Target {
        self.guard.get(&self.key).unwrap()
    }
}

impl<'a, K, V, S> Entry<'a, K, V, S>
where
    K: Eq + Hash + Clone,
    S: BuildHasher,
{
    pub fn or_default(self) -> WriteGuard<'a, K, V, S>
    where
        V: Default,
    {
        match self {
            Entry::Occupied(o) => WriteGuard {
                guard: o.guard,
                key: o.key,
            },
            Entry::Vacant(mut v) => {
                v.guard.insert(v.key.clone(), V::default());
                WriteGuard {
                    guard: v.guard,
                    key: v.key,
                }
            }
        }
    }

    pub fn into_mut(self) -> Option<WriteGuard<'a, K, V, S>> {
        match self {
            Entry::Occupied(o) => Some(WriteGuard {
                guard: o.guard,
                key: o.key,
            }),
            Entry::Vacant(_) => None,
        }
    }
}

pub struct WriteGuard<'a, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    guard: RwLockWriteGuard<'a, HashMap<K, V, S>>,
    key: K,
}

impl<K, V, S> Deref for WriteGuard<'_, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    type Target = V;
    fn deref(&self) -> &Self::Target {
        self.guard.get(&self.key).unwrap()
    }
}

impl<K, V, S> DerefMut for WriteGuard<'_, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.get_mut(&self.key).unwrap()
    }
}

impl<K, V, S> WriteGuard<'_, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    pub fn value(&self) -> &V {
        self.guard.get(&self.key).unwrap()
    }
}

#[cfg(test)]
mod tests {
    // this is NOT a shuttle test, just a regular unit test
    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn shuttle_map_smoke() {
        use super::ShuttleMap;

        let map = ShuttleMap::new(4);
        assert!(map.is_empty());
        {
            let mut e = map.entry(1u8).or_default();
            *e = 10;
        }
        assert!(!map.is_empty());
        assert_eq!(*map.get(&1u8).unwrap(), 10);

        map.retain(|k, _v| *k != 1u8);
        assert!(map.is_empty());
    }
}
