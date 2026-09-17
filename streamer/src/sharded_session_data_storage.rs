//! This module implements [`ShardedSessionDataStorage`], a concurrent set-associative data
//! structure that is optimized for the access patterns of TLS session storage. The design is
//! inspired by a similar data structure implemented in NSS, see
//! https://github.com/nss-dev/nss/blob/master/lib/ssl/sslsnce.c.
//!
//! TLS session storage relies on a cache of session tickets identified by session ID. Each ticket
//! is used only once, which means a generic LRU cache is overkill and a FIFO strategy is enough.
//! The hot path is `take()`, which looks up a session ID and invalidates the entry on hit.
//!
//! The default implementation of the session data storage `ServerSessionMemoryCache` in rustls
//! performs poorly under load for several reasons:
//! * it uses a remove path that is O(n) (noted for the TLS 1.3 path);
//! * it uses one mutex for all operations, so at large cache sizes, lock hold time can increase and
//!   cause contention/perf collapse under concurrency;
//!
//! The `ShardedSessionDataStorage` implementation provides 5x-10x throughput improvements over
//! `ServerSessionMemoryCache` in our benchmarks under concurrent scenarios with cache sizes of
//! 4K-16K entries.
//!
//! # Example
//!
//! ```rust
//! use rustls::server::StoresServerSessions;
//! use session_data_storage::ShardedSessionDataStorageV3;
//!
//! let cache = ShardedSessionDataStorageV3::new(4096);
//! let sid = vec![0x01, 0x02, 0x03];
//! let ticket = vec![0xAA, 0xBB];
//!
//! assert!(cache.can_cache());
//! assert!(cache.put(sid.clone(), ticket.clone()));
//! assert_eq!(cache.get(&sid), Some(ticket.clone()));
//! assert_eq!(cache.take(&sid), Some(ticket));
//! assert_eq!(cache.take(&sid), None);
//! ```
//! # Implementation details
//!
//! The cache is split into sets of fixed size `SHARD_CAPACITY`. Each set is protected by its own
//! mutex. The key/value data is stored in a contiguous array of `SessionDataSlot` aligned by cache
//! line.
//!
//! For lookup optimization, we use array of `Tag` to store a short fingerprint of the key and its
//! length. Each tag stores the first 3 bytes of the key: under non-adversarial (uniform-random)
//! keys, the probability that one lookup collides with any tag in a 128-entry set is about 128 /
//! 2^24 = 0.000763%. Probability of any collision (birthday bound) among 128 tags is about 0.048%.
//! Tag also stores the key length and invalid tags are zeroed out.
//!
//! The data structure contains arrays that are allocated once, avoiding any heap allocations
//! during method calls.
use {
    rustls::server::StoresServerSessions,
    std::{
        cell::UnsafeCell,
        fmt,
        hash::{BuildHasher, Hasher},
        sync::{Arc, Mutex},
    },
};

/// The number of entries per shard.
pub const SHARD_CAPACITY: usize = 128;
/// Rustls uses session id as key and bounds them to be at most 32 bytes.
const SESSION_ID_MAX_LEN: usize = 32;

/// [`ShardedSessionDataStorage`] is a concurrent set-associative data structure optimized for the
/// access patterns of TLS session storage.
///
/// This cache is internally composed of equally sized shards, each of which is independently
/// synchronized. This allows for low contention when multiple threads are accessing the cache but limits the
/// maximum weight capacity of each shard. The size of each shard is fixed at `SHARD_CAPACITY` (128)
/// entries.
pub struct ShardedSessionDataStorage {
    // Use `Box<[UnsafeCell<T>]>` because rustls `StoresServerSessions` trait methods take `&self`,
    // but we need interior mutability.
    tags: Box<[UnsafeCell<Tag>]>,
    data: Box<[UnsafeCell<SessionDataSlot>]>,
    // Each mutex protects not only `SessionDataShard` instance but also corresponding slice of
    // `tags` and `data` for a shard. To avoid false sharing, we could pad to cache
    // line but we haven't observed significant performance improvements.
    locks: Box<[Mutex<SessionDataShard>]>,
    hash_builder: ahash::RandomState,
}

impl fmt::Debug for ShardedSessionDataStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShardedSessionDataStorage")
            .field("entry_capacity", &self.data.len())
            .field("shard_count", &self.locks.len())
            .field("shard_capacity", &SHARD_CAPACITY)
            .finish_non_exhaustive()
    }
}

// SAFETY: all mutable access to `data` and `tags` is synchronized by the corresponding per-shard
// mutex in `locks`.
unsafe impl Sync for ShardedSessionDataStorage {}

impl StoresServerSessions for ShardedSessionDataStorage {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool {
        if !Self::key_is_supported(&key) {
            return false;
        }

        let shard_index = self.shard_index(&key);
        let mut shard = self.put_lock_shard(shard_index);
        shard.write(&key, value);
        true
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.lookup_value(key, false)
    }

    fn take(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.lookup_value(key, true)
    }

    fn can_cache(&self) -> bool {
        true
    }
}

impl ShardedSessionDataStorage {
    pub fn new(size: usize) -> Arc<Self> {
        let hash_builder: ahash::RandomState = ahash::RandomState::new();
        Self::with_hash_builder(size, hash_builder)
    }

    fn with_hash_builder(size: usize, hash_builder: ahash::RandomState) -> Arc<Self> {
        let size = size.max(1).div_ceil(SHARD_CAPACITY) * SHARD_CAPACITY;

        let shard_count = size / SHARD_CAPACITY;

        let tags = (0..size)
            .map(|_| UnsafeCell::new(Tag::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let data = (0..size)
            .map(|_| UnsafeCell::new(SessionDataSlot::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let locks = (0..shard_count)
            .map(|_| Mutex::new(SessionDataShard::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Arc::new(Self {
            tags,
            data,
            locks,
            hash_builder,
        })
    }

    fn shard_index(&self, key: &[u8]) -> usize {
        let shard_count = self.locks.len();

        let mut hasher = self.hash_builder.build_hasher();
        hasher.write(key);
        (hasher.finish() as usize) % shard_count
    }

    #[inline]
    fn key_is_supported(key: &[u8]) -> bool {
        !key.is_empty() && key.len() <= SESSION_ID_MAX_LEN
    }

    /// Looks up the value for the given key. If `invalidate_on_hit` is true, the entry will be
    /// invalidated when a match is found.
    ///
    /// The `key` is expected to be non-empty and at most `SESSION_ID_MAX_LEN` bytes long.
    fn lookup_value(&self, key: &[u8], invalidate_on_hit: bool) -> Option<Vec<u8>> {
        if !Self::key_is_supported(key) {
            return None;
        }

        let shard_index = self.shard_index(key);
        let (mut shard, mut cursor) = self.lookup_lock_shard(shard_index);

        let expected_tag = Tag::packed_word_for_key(key);

        // Hot-path optimization: for 32-byte keys, compare against a fixed-size array.
        let key32 = if key.len() == SESSION_ID_MAX_LEN {
            // SAFETY: key_len == SESSION_ID_MAX_LEN guarantees enough bytes.
            Some(unsafe { key_as_32_unchecked(key) })
        } else {
            None
        };

        // Collect matching candidate slots in recency order.
        let mut candidates = [0_u8; SHARD_CAPACITY];
        let mut candidate_count = 0_usize;
        for _ in 0..SHARD_CAPACITY {
            cursor.prev();

            // SAFETY: ndx is within this set's fixed contiguous region.
            let tag = shard.tag(cursor);
            if *tag == expected_tag {
                debug_assert!(candidate_count < SHARD_CAPACITY);
                // SAFETY: candidate_count increases at most once per loop iteration, and this loop
                // executes exactly SHARD_CAPACITY iterations. This optimization to remove
                // panic_bounds_check from hot path.
                unsafe {
                    *candidates.get_unchecked_mut(candidate_count) = cursor.position as u8;
                }
                candidate_count += 1;
            }
        }

        if candidate_count == 0 {
            return None;
        }

        for &candidate in &candidates[..candidate_count] {
            let cursor = LookupCursor {
                position: candidate as usize,
            };

            if shard.matches_key(cursor, key32, key) {
                if invalidate_on_hit {
                    return Some(shard.take_value_and_invalidate(cursor));
                }
                // this is the only place where we allocate
                return Some(shard.clone_value(cursor));
            }
        }

        None
    }

    #[inline(always)]
    fn lock_shard(
        &self,
        shard_index: usize,
    ) -> (std::sync::MutexGuard<'_, SessionDataShard>, usize, usize) {
        let guard = match self.locks[shard_index].lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let base = shard_index * SHARD_CAPACITY;
        let end = base + SHARD_CAPACITY;
        debug_assert!(end <= self.tags.len());
        debug_assert!(end <= self.data.len());
        (guard, base, end)
    }

    fn put_lock_shard(&self, shard_index: usize) -> PutShardGuard<'_> {
        let (guard, base, end) = self.lock_shard(shard_index);

        PutShardGuard {
            guard,
            tags: &self.tags[base..end],
            data: &self.data[base..end],
        }
    }

    fn lookup_lock_shard(&self, shard_index: usize) -> (LookupShardGuard<'_>, LookupCursor) {
        let (guard, base, end) = self.lock_shard(shard_index);
        // SAFETY: mutable shard metadata access is serialized by `locks[shard_index]`.
        let position = guard.next;

        (
            LookupShardGuard {
                _guard: guard,
                tags: &self.tags[base..end],
                data: &self.data[base..end],
            },
            LookupCursor { position },
        )
    }
}

#[inline(always)]
unsafe fn key_as_32_unchecked(key: &[u8]) -> &[u8; SESSION_ID_MAX_LEN] {
    debug_assert_eq!(key.len(), SESSION_ID_MAX_LEN);
    &*(key.as_ptr() as *const [u8; SESSION_ID_MAX_LEN])
}

struct PutShardGuard<'a> {
    guard: std::sync::MutexGuard<'a, SessionDataShard>,
    tags: &'a [UnsafeCell<Tag>],
    data: &'a [UnsafeCell<SessionDataSlot>],
}

impl PutShardGuard<'_> {
    #[inline(always)]
    fn write(&mut self, key: &[u8], value: Vec<u8>) {
        let tag = self.tag_mut();
        tag.set_from_key_slice(key);

        let slot = self.slot_mut();
        slot.key[..key.len()].copy_from_slice(key);
        slot.value = value;

        self.guard.next = (self.guard.next + 1) % SHARD_CAPACITY;
    }

    #[inline(always)]
    fn tag_mut(&mut self) -> &mut Tag {
        unsafe { &mut *self.tags.get_unchecked(self.guard.next).get() }
    }

    #[inline(always)]
    fn slot_mut(&mut self) -> &mut SessionDataSlot {
        unsafe { &mut *self.data.get_unchecked(self.guard.next).get() }
    }
}

/// [`LookupCursor`] is a structure to navigate through the entries in a shard during lookup. We
/// search backwards from the next insertion position.
#[derive(Debug, Clone, Copy)]
struct LookupCursor {
    position: usize,
}

impl LookupCursor {
    #[inline(always)]
    fn position(&self) -> usize {
        self.position
    }

    #[inline(always)]
    fn prev(&mut self) {
        self.position = self.position.wrapping_sub(1) % SHARD_CAPACITY;
    }
}

struct LookupShardGuard<'a> {
    _guard: std::sync::MutexGuard<'a, SessionDataShard>,
    tags: &'a [UnsafeCell<Tag>],
    data: &'a [UnsafeCell<SessionDataSlot>],
}

impl LookupShardGuard<'_> {
    #[inline(always)]
    fn tag(&self, cursor: LookupCursor) -> &Tag {
        unsafe { &*self.tags.get_unchecked(cursor.position()).get() }
    }

    #[inline(always)]
    fn matches_key(
        &self,
        cursor: LookupCursor,
        key32: Option<&[u8; SESSION_ID_MAX_LEN]>,
        key: &[u8],
    ) -> bool {
        let slot = self.slot(cursor);
        if let Some(key32) = key32 {
            slot.key == *key32
        } else {
            // SAFETY: key_len <= SESSION_ID_MAX_LEN and key_len is the comparison length.
            unsafe { std::slice::from_raw_parts(slot.key.as_ptr(), key.len()) == key }
        }
    }

    #[inline(always)]
    fn clone_value(&self, cursor: LookupCursor) -> Vec<u8> {
        let slot = self.slot(cursor);
        slot.value.clone()
    }

    #[inline(always)]
    fn take_value_and_invalidate(&mut self, cursor: LookupCursor) -> Vec<u8> {
        let tag = self.tag_mut(cursor);
        tag.invalidate();
        std::mem::take(&mut self.slot_mut(cursor).value)
    }

    #[inline(always)]
    fn tag_mut(&mut self, cursor: LookupCursor) -> &mut Tag {
        unsafe { &mut *self.tags.get_unchecked(cursor.position()).get() }
    }

    #[inline(always)]
    fn slot_mut(&mut self, cursor: LookupCursor) -> &mut SessionDataSlot {
        unsafe { &mut *self.data.get_unchecked(cursor.position()).get() }
    }

    #[inline(always)]
    fn slot(&self, cursor: LookupCursor) -> &SessionDataSlot {
        unsafe { &*self.data.get_unchecked(cursor.position()).get() }
    }
}

#[repr(transparent)]
#[derive(Copy, Clone, Default, Eq, PartialEq)]
pub(crate) struct Tag(u32);

impl Tag {
    #[inline]
    fn len_byte(len: usize) -> u32 {
        debug_assert!(len <= SESSION_ID_MAX_LEN);
        len as u32
    }

    #[inline]
    fn pack_prefix_3_bytes(key: &[u8]) -> u32 {
        debug_assert!(key.len() <= SESSION_ID_MAX_LEN);
        // Fast path for the common rustls SessionId length.
        if key.len() == SESSION_ID_MAX_LEN {
            return (key[0] as u32) | ((key[1] as u32) << 8) | ((key[2] as u32) << 16);
        }

        let mut prefix = 0_u32;
        if !key.is_empty() {
            prefix |= key[0] as u32;
        }
        if key.len() > 1 {
            prefix |= (key[1] as u32) << 8;
        }
        if key.len() > 2 {
            prefix |= (key[2] as u32) << 16;
        }
        prefix
    }

    #[inline]
    fn packed_word_for_key(key: &[u8]) -> Self {
        let prefix = Self::pack_prefix_3_bytes(key);
        let meta = Self::len_byte(key.len());
        Self(prefix | (meta << 24))
    }

    #[inline]
    fn set_from_key_slice(&mut self, key: &[u8]) {
        *self = Self::packed_word_for_key(key);
    }

    #[inline]
    fn invalidate(&mut self) {
        self.0 = 0;
    }
}

#[repr(align(64))]
#[derive(Debug, Default)]
struct SessionDataSlot {
    key: [u8; SESSION_ID_MAX_LEN],
    value: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy)]
struct SessionDataShard {
    // next insertion position per shard.
    next: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(size: usize) -> Arc<ShardedSessionDataStorage> {
        ShardedSessionDataStorage::new(size)
    }

    #[test]
    fn test_sharded_session_data_storage_accepts_put() {
        let c = cache(SHARD_CAPACITY);
        assert!(c.put(vec![0x01], vec![0x02]));
    }

    #[test]
    fn test_sharded_session_data_storage_rejects_empty_key() {
        let c = cache(SHARD_CAPACITY);
        assert!(!c.put(vec![], vec![0x02]));
        assert_eq!(c.get(&[]), None);
        assert_eq!(c.take(&[]), None);
    }

    #[test]
    fn test_sharded_session_data_storage_persists_put() {
        let c = cache(SHARD_CAPACITY);
        assert!(c.put(vec![0x01], vec![0x02]));
        assert_eq!(c.get(&[0x01]), Some(vec![0x02]));
        assert_eq!(c.get(&[0x01]), Some(vec![0x02]));
    }

    #[test]
    fn test_sharded_session_data_storage_overwrites_put() {
        let c = cache(SHARD_CAPACITY);
        assert!(c.put(vec![0x01], vec![0x02]));
        assert!(c.put(vec![0x01], vec![0x04]));
        assert_eq!(c.get(&[0x01]), Some(vec![0x04]));
    }

    #[test]
    fn test_sharded_session_data_storage_take_removes_value() {
        let c = cache(SHARD_CAPACITY);
        assert!(c.put(vec![0x01], vec![0x02]));
        assert_eq!(c.take(&[0x01]), Some(vec![0x02]));
        assert_eq!(c.get(&[0x01]), None);
        assert_eq!(c.take(&[0x01]), None);
    }

    #[test]
    fn test_sharded_session_data_storage_drops_to_maintain_size_invariant() {
        let c = cache(SHARD_CAPACITY);
        let inserted = SHARD_CAPACITY * 2;

        for i in 0..inserted {
            assert!(c.put(vec![i as u8], vec![(i.wrapping_add(1)) as u8]));
        }

        let present = (0..inserted)
            .filter(|i| c.get(&[*i as u8]).is_some())
            .count();

        assert!(present < inserted, "cache should evict older entries");
    }

    fn tag_packed_word(tag: Tag) -> u32 {
        tag.0
    }
    fn tag_is_valid(tag: Tag) -> bool {
        tag.0 != 0
    }
    fn tag_len(tag: Tag) -> usize {
        ((tag.0 >> 24) & 0xFF) as usize
    }

    #[test]
    fn test_tag_layout_is_4_bytes() {
        assert_eq!(std::mem::size_of::<Tag>(), 4);
    }

    #[test]
    fn test_tag_invalidate_zeros_whole_tag() {
        let mut tag = Tag::default();
        tag.set_from_key_slice(&[1, 2, 3, 4]);
        assert_ne!(tag_packed_word(tag), 0);

        tag.invalidate();
        assert_eq!(tag_packed_word(tag), 0);
        assert!(!tag_is_valid(tag));
        assert_eq!(tag_len(tag), 0);
    }

    #[test]
    fn test_tag_packed_word_is_deterministic_for_same_key() {
        let key = [9_u8, 8, 7, 6, 5, 4, 3, 2];
        let first = Tag::packed_word_for_key(&key);
        let second = Tag::packed_word_for_key(&key);
        assert_eq!(tag_packed_word(first), tag_packed_word(second));
    }

    #[test]
    fn test_tag_packed_word_changes_when_prefix_changes() {
        let key_a = [1_u8, 2, 3, 4, 5, 6];
        let key_b = [7_u8, 2, 3, 4, 5, 6];
        let tag_a = Tag::packed_word_for_key(&key_a);
        let tag_b = Tag::packed_word_for_key(&key_b);
        assert_ne!(tag_packed_word(tag_a), tag_packed_word(tag_b));
    }

    #[test]
    fn test_tag_packed_word_changes_when_only_length_changes() {
        let key_a = [1_u8, 2, 3];
        let key_b = [1_u8, 2, 3, 0];
        let tag_a = Tag::packed_word_for_key(&key_a);
        let tag_b = Tag::packed_word_for_key(&key_b);
        assert_ne!(tag_packed_word(tag_a), tag_packed_word(tag_b));
    }

    #[test]
    fn test_tag_packed_word_encodes_first_three_bytes_and_len_for_32_byte_key() {
        let mut key = [0_u8; SESSION_ID_MAX_LEN];
        key[0] = 0xAB;
        key[1] = 0xCD;
        key[2] = 0xEF;

        let tag = Tag::packed_word_for_key(&key);
        let expected = 0xAB_u32 | (0xCD_u32 << 8) | (0xEF_u32 << 16) | (32_u32 << 24);
        assert_eq!(tag_packed_word(tag), expected);
        assert_eq!(tag_len(tag), 32);
    }

    #[test]
    fn test_sharded_session_data_storage_rejects_too_long_key() {
        let c = cache(SHARD_CAPACITY);
        let too_long = vec![0xAB; SESSION_ID_MAX_LEN + 1];

        assert!(!c.put(too_long.clone(), vec![0x01]));
        assert_eq!(c.get(&too_long), None);
        assert_eq!(c.take(&too_long), None);
    }
}
