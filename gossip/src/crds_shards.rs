use {
    crate::{crds::VersionedCrdsValue, crds_gossip_pull::CrdsFilter},
    indexmap::map::IndexMap,
    std::{
        cmp::Ordering,
        ops::{Index, IndexMut},
    },
};

#[derive(Clone)]
pub struct CrdsShards {
    // shards[k] includes crds values which the first shard_bits of their hash
    // value is equal to k. Each shard is a mapping from crds values indices to
    // their hash value.
    shards: Vec<IndexMap<usize, u64>>,
    shard_bits: u32,
}

impl CrdsShards {
    pub fn new(shard_bits: u32) -> Self {
        CrdsShards {
            shards: vec![IndexMap::new(); 1 << shard_bits],
            shard_bits,
        }
    }

    pub fn insert(&mut self, index: usize, value: &VersionedCrdsValue) -> bool {
        let hash = CrdsFilter::hash_as_u64(value.value.hash());
        self.shard_mut(hash).insert(index, hash).is_none()
    }

    pub fn remove(&mut self, index: usize, value: &VersionedCrdsValue) -> bool {
        let hash = CrdsFilter::hash_as_u64(value.value.hash());
        self.shard_mut(hash).swap_remove(&index).is_some()
    }

    /// Returns indices of all crds values which the first 'mask_bits' of their
    /// hash value is equal to 'mask'.
    pub fn find(&self, mask: u64, mask_bits: u32) -> impl Iterator<Item = usize> + '_ {
        let mask = CrdsFilter::canonical_mask(mask, mask_bits);
        match self.shard_bits.cmp(&mask_bits) {
            Ordering::Less => {
                let pred = move |(&index, &hash): (&usize, &u64)| {
                    if CrdsFilter::hash_matches_mask_prefix(mask, mask_bits, hash) {
                        Some(index)
                    } else {
                        None
                    }
                };
                Iter::Less(self.shard(mask).iter().filter_map(pred))
            }
            Ordering::Equal => Iter::Equal(self.shard(mask).keys().cloned()),
            Ordering::Greater => {
                let count = 1 << (self.shard_bits - mask_bits);
                let end = self.shard_index(mask) + 1;
                Iter::Greater(
                    self.shards[end - count..end]
                        .iter()
                        .flat_map(IndexMap::keys)
                        .cloned(),
                )
            }
        }
    }

    pub(crate) fn find_count(&self, mask: u64, mask_bits: u32) -> usize {
        let mask = CrdsFilter::canonical_mask(mask, mask_bits);
        match self.shard_bits.cmp(&mask_bits) {
            Ordering::Less | Ordering::Equal => self.shard(mask).len(),
            Ordering::Greater => {
                let count = 1 << (self.shard_bits - mask_bits);
                let end = self.shard_index(mask) + 1;
                self.shards[end - count..end]
                    .iter()
                    .map(IndexMap::len)
                    .sum()
            }
        }
    }

    #[inline]
    fn shard_index(&self, hash: u64) -> usize {
        hash.checked_shr(64 - self.shard_bits).unwrap_or(0) as usize
    }

    #[inline]
    fn shard(&self, hash: u64) -> &IndexMap<usize, u64> {
        let shard_index = self.shard_index(hash);
        self.shards.index(shard_index)
    }

    #[inline]
    fn shard_mut(&mut self, hash: u64) -> &mut IndexMap<usize, u64> {
        let shard_index = self.shard_index(hash);
        self.shards.index_mut(shard_index)
    }

    // Checks invariants in the shards tables against the crds table.
    #[cfg(test)]
    pub fn check(&self, crds: &[VersionedCrdsValue]) {
        let mut indices: Vec<_> = self
            .shards
            .iter()
            .flat_map(IndexMap::keys)
            .cloned()
            .collect();
        indices.sort_unstable();
        assert_eq!(indices, (0..crds.len()).collect::<Vec<_>>());
        for (shard_index, shard) in self.shards.iter().enumerate() {
            for (&index, &hash) in shard {
                assert_eq!(hash, CrdsFilter::hash_as_u64(crds[index].value.hash()));
                assert_eq!(
                    shard_index as u64,
                    hash.checked_shr(64 - self.shard_bits).unwrap_or(0)
                );
            }
        }
    }
}

// Wrapper for 3 types of iterators we get when comparing shard_bits and
// mask_bits in find method. This is to avoid Box<dyn Iterator<Item =...>>
// which involves dynamic dispatch and is relatively slow.
enum Iter<R, S, T> {
    Less(R),
    Equal(S),
    Greater(T),
}

impl<R, S, T> Iterator for Iter<R, S, T>
where
    R: Iterator<Item = usize>,
    S: Iterator<Item = usize>,
    T: Iterator<Item = usize>,
{
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Greater(iter) => iter.next(),
            Self::Less(iter) => iter.next(),
            Self::Equal(iter) => iter.next(),
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::{
            crds::{Crds, GossipRoute},
            crds_value::CrdsValue,
        },
        rand::{Rng, rng},
        solana_time_utils::timestamp,
        std::{collections::HashSet, iter::repeat_with, ops::Index},
    };

    fn new_test_crds_value<R: Rng>(rng: &mut R) -> VersionedCrdsValue {
        let value = CrdsValue::new_rand(rng, None);
        let label = value.label();
        let mut crds = Crds::default();
        crds.insert(value, timestamp(), GossipRoute::LocalMessage)
            .unwrap();
        crds.get::<&VersionedCrdsValue>(&label).cloned().unwrap()
    }

    // Returns true if the first mask_bits most significant bits of hash is the
    // same as the given bit mask.
    fn check_mask(value: &VersionedCrdsValue, mask: u64, mask_bits: u32) -> bool {
        let hash = CrdsFilter::hash_as_u64(value.value.hash());
        let ones = (!0u64).checked_shr(mask_bits).unwrap_or(0u64);
        (hash | ones) == (mask | ones)
    }

    // Manual filtering by scanning all the values.
    fn filter_crds_values(
        values: &[VersionedCrdsValue],
        mask: u64,
        mask_bits: u32,
    ) -> HashSet<usize> {
        values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                if check_mask(value, mask, mask_bits) {
                    Some(index)
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn test_crds_shards_round_trip() {
        let mut rng = rng();
        // Generate some random hash and crds value labels.
        let mut values: Vec<_> = repeat_with(|| new_test_crds_value(&mut rng))
            .take(4096)
            .collect();
        // Insert everything into the crds shards.
        let mut shards = CrdsShards::new(5);
        for (index, value) in values.iter().enumerate() {
            assert!(shards.insert(index, value));
        }
        shards.check(&values);
        // Remove some of the values.
        for _ in 0..512 {
            let index = rng.random_range(0..values.len());
            let value = values.swap_remove(index);
            assert!(shards.remove(index, &value));
            if index < values.len() {
                let value = values.index(index);
                assert!(shards.remove(values.len(), value));
                assert!(shards.insert(index, value));
            }
            shards.check(&values);
        }
        const SHARD_BITS: u32 = 5;
        for _ in 0..10 {
            let mask = rng.random();
            for mask_bits in 0..12 {
                let mut set = filter_crds_values(&values, mask, mask_bits);
                let visited = filter_crds_values(&values, mask, mask_bits.min(SHARD_BITS)).len();
                assert_eq!(
                    shards.find_count(mask, mask_bits),
                    visited,
                    "find_count should equal visited entries for mask_bits={mask_bits}"
                );
                for index in shards.find(mask, mask_bits) {
                    assert!(set.remove(&index));
                }
                assert!(set.is_empty());
            }
        }
        // Existing hash values.
        for (index, value) in values.iter().enumerate() {
            let mask = CrdsFilter::hash_as_u64(value.value.hash());
            let hits: Vec<_> = shards.find(mask, 64).collect();
            assert_eq!(hits, vec![index]);
        }
        // Remove everything.
        while !values.is_empty() {
            let index = rng.random_range(0..values.len());
            let value = values.swap_remove(index);
            assert!(shards.remove(index, &value));
            if index < values.len() {
                let value = values.index(index);
                assert!(shards.remove(values.len(), value));
                assert!(shards.insert(index, value));
            }
            if index % 5 == 0 {
                shards.check(&values);
            }
        }
    }

    #[test]
    fn test_find_count_reflects_prefix_skew() {
        const SHARD_BITS: u32 = 4;
        const HOT: usize = 100;
        const COLD: usize = 40;
        let mut rng = rng();
        let mut shards = CrdsShards::new(SHARD_BITS);
        let mut values: Vec<VersionedCrdsValue> = Vec::new();
        let prefix = |v: &VersionedCrdsValue| {
            CrdsFilter::hash_as_u64(v.value.hash())
                .checked_shr(64 - SHARD_BITS)
                .unwrap_or(0)
        };
        while values.iter().filter(|v| prefix(v) == 0).count() < HOT {
            let v = new_test_crds_value(&mut rng);
            if prefix(&v) == 0 {
                let index = values.len();
                assert!(shards.insert(index, &v));
                values.push(v);
            }
        }
        let mut cold = 0;
        while cold < COLD {
            let v = new_test_crds_value(&mut rng);
            if prefix(&v) != 0 {
                let index = values.len();
                assert!(shards.insert(index, &v));
                values.push(v);
                cold += 1;
            }
        }
        let crds_len = values.len();
        let scanned = shards.find_count(0, SHARD_BITS);
        assert_eq!(scanned, HOT, "hot prefix must scan all its entries");
        assert_eq!(scanned, shards.find(0, SHARD_BITS).count());
        let average_estimate = (crds_len >> SHARD_BITS).max(1);
        assert!(
            scanned > average_estimate * 8,
            "find_count={scanned} should dwarf average estimate={average_estimate}",
        );
    }

    #[test]
    fn test_find_count_charges_full_shard_for_fine_mask() {
        const SHARD_BITS: u32 = 4;
        const HOT: usize = 100;
        let mut rng = rng();
        let mut shards = CrdsShards::new(SHARD_BITS);
        let mut values: Vec<VersionedCrdsValue> = Vec::new();
        let prefix = |v: &VersionedCrdsValue| {
            CrdsFilter::hash_as_u64(v.value.hash())
                .checked_shr(64 - SHARD_BITS)
                .unwrap_or(0)
        };
        while values.iter().filter(|v| prefix(v) == 0).count() < HOT {
            let v = new_test_crds_value(&mut rng);
            if prefix(&v) == 0 {
                let index = values.len();
                assert!(shards.insert(index, &v));
                values.push(v);
            }
        }
        let absent = (!0u64).checked_shr(SHARD_BITS).unwrap_or(0);
        assert!(
            !values
                .iter()
                .any(|v| CrdsFilter::hash_as_u64(v.value.hash()) == absent),
            "sentinel hash must be absent",
        );
        assert_eq!(shards.find(absent, 64).count(), 0, "filter matches nothing");
        assert_eq!(
            shards.find_count(absent, 64),
            HOT,
            "must charge for the full physical shard scanned, not the zero matches",
        );
    }
}
