use {
    crate::shred::{
        self, Error, ProcessShredsStats, Shred, ShredData, ShredFlags, DATA_SHREDS_PER_FEC_BLOCK,
    },
    lazy_lru::LruCache,
    rayon::ThreadPool,
    reed_solomon_erasure::{galois_8::ReedSolomon, Error::TooFewDataShards},
    solana_clock::Slot,
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_rayon_threadlimit::get_thread_count,
    std::{
        fmt::Debug,
        sync::{Arc, OnceLock, RwLock},
        time::Instant,
    },
};

static PAR_THREAD_POOL: std::sync::LazyLock<ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(get_thread_count())
        .thread_name(|i| format!("solShredder{i:02}"))
        .build()
        .unwrap()
});

// Arc<...> wrapper so that cache entries can be initialized without locking
// the entire cache.
type LruCacheOnce<K, V> = RwLock<LruCache<K, Arc<OnceLock<V>>>>;

pub struct ReedSolomonCache(
    LruCacheOnce<
        (usize, usize), // number of {data,parity} shards
        Result<Arc<ReedSolomon>, reed_solomon_erasure::Error>,
    >,
);

#[derive(Debug)]
pub struct Shredder {
    slot: Slot,
    parent_slot: Slot,
    version: u16,
    reference_tick: u8,
}

impl Shredder {
    pub fn new(
        slot: Slot,
        parent_slot: Slot,
        reference_tick: u8,
        version: u16,
    ) -> Result<Self, Error> {
        if slot < parent_slot || slot - parent_slot > u64::from(u16::MAX) {
            Err(Error::InvalidParentSlot { slot, parent_slot })
        } else {
            Ok(Self {
                slot,
                parent_slot,
                reference_tick,
                version,
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn make_merkle_shreds_from_entries(
        &self,
        keypair: &Keypair,
        entries: &[Entry],
        is_last_in_slot: bool,
        chained_merkle_root: Hash,
        next_shred_index: u32,
        next_code_index: u32,
        reed_solomon_cache: &ReedSolomonCache,
        stats: &mut ProcessShredsStats,
    ) -> impl Iterator<Item = Shred> + use<> {
        let now = Instant::now();
        let entries = wincode::serialize(entries).unwrap();
        stats.serialize_elapsed += now.elapsed().as_micros() as u64;
        Self::make_shreds_from_data_slice(
            self,
            keypair,
            &entries,
            is_last_in_slot,
            chained_merkle_root,
            next_shred_index,
            next_code_index,
            reed_solomon_cache,
            stats,
        )
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn make_shreds_from_data_slice(
        &self,
        keypair: &Keypair,
        data: &[u8],
        is_last_in_slot: bool,
        chained_merkle_root: Hash,
        next_shred_index: u32,
        next_code_index: u32,
        reed_solomon_cache: &ReedSolomonCache,
        stats: &mut ProcessShredsStats,
    ) -> Result<impl Iterator<Item = Shred> + use<>, Error> {
        let thread_pool: &ThreadPool = &PAR_THREAD_POOL;
        let shreds = shred::merkle::make_shreds_from_data(
            thread_pool,
            keypair,
            chained_merkle_root,
            data,
            self.slot,
            self.parent_slot,
            self.version,
            self.reference_tick,
            is_last_in_slot,
            next_shred_index,
            next_code_index,
            reed_solomon_cache,
            stats,
        )?;
        Ok(shreds.into_iter().map(Shred::from))
    }

    pub fn entries_to_merkle_shreds_for_tests(
        &self,
        keypair: &Keypair,
        entries: &[Entry],
        is_last_in_slot: bool,
        chained_merkle_root: Hash,
        next_shred_index: u32,
        next_code_index: u32,
        reed_solomon_cache: &ReedSolomonCache,
        stats: &mut ProcessShredsStats,
    ) -> (
        Vec<Shred>, // data shreds
        Vec<Shred>, // coding shreds
    ) {
        self.make_merkle_shreds_from_entries(
            keypair,
            entries,
            is_last_in_slot,
            chained_merkle_root,
            next_shred_index,
            next_code_index,
            reed_solomon_cache,
            stats,
        )
        .partition(Shred::is_data)
    }

    /// Combines all shreds to recreate the original buffer
    pub fn deshred<I, T: AsRef<[u8]>>(shreds: I) -> Result<Vec<u8>, Error>
    where
        I: IntoIterator<Item = T>,
    {
        let (data, _, data_complete) = shreds.into_iter().try_fold(
            <(Vec<u8>, Option<u32>, bool)>::default(),
            |(mut data, prev, data_complete), shred| {
                // No trailing shreds if we have already observed
                // DATA_COMPLETE_SHRED.
                if data_complete {
                    return Err(Error::InvalidDeshredSet);
                }
                let shred = shred.as_ref();
                // Shreds' indices should be consecutive.
                let index = Some(
                    shred::layout::get_index(shred)
                        .ok_or_else(|| Error::InvalidPayloadSize(shred.len()))?,
                );
                if let Some(prev) = prev {
                    if prev.checked_add(1) != index {
                        return Err(Error::from(TooFewDataShards));
                    }
                }
                data.extend_from_slice(shred::layout::get_data(shred)?);
                let flags = shred::layout::get_flags(shred)?;
                let data_complete = flags.contains(ShredFlags::DATA_COMPLETE_SHRED);
                Ok((data, index, data_complete))
            },
        )?;
        // The last shred should be DATA_COMPLETE_SHRED.
        if !data_complete {
            return Err(Error::from(TooFewDataShards));
        }
        if data.is_empty() {
            // For backward compatibility. This is needed when the data shred
            // payload is None, so that deserializing to Vec<Entry> results in
            // an empty vector.
            let data_buffer_size =
                ShredData::capacity(/*proof_size:*/ 0, /*resigned:*/ false).unwrap();
            Ok(vec![0u8; data_buffer_size])
        } else {
            Ok(data)
        }
    }
    /// Produce a single shred with no payload
    /// for use in tests and such
    #[cfg(feature = "dev-context-only-utils")]
    pub fn single_shred_for_tests(slot: Slot, keypair: &Keypair) -> Shred {
        let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 42).unwrap();
        let reed_solomon_cache = ReedSolomonCache::default();
        let (mut shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            keypair,
            &[],
            true,
            Hash::default(),
            0,
            0,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );
        shreds.pop().unwrap()
    }
}

impl ReedSolomonCache {
    const CAPACITY: usize = 4 * DATA_SHREDS_PER_FEC_BLOCK;

    pub(crate) fn get(
        &self,
        data_shards: usize,
        parity_shards: usize,
    ) -> Result<Arc<ReedSolomon>, reed_solomon_erasure::Error> {
        let key = (data_shards, parity_shards);
        // Read from the cache with a shared lock.
        let entry = self.0.read().unwrap().get(&key).cloned();
        // Fall back to exclusive lock if there is a cache miss.
        let entry: Arc<OnceLock<Result<_, _>>> = entry.unwrap_or_else(|| {
            let mut cache = self.0.write().unwrap();
            cache.get(&key).cloned().unwrap_or_else(|| {
                let entry = Arc::<OnceLock<Result<_, _>>>::default();
                cache.put(key, Arc::clone(&entry));
                entry
            })
        });
        // Initialize if needed by only a single thread outside locks.
        entry
            .get_or_init(|| ReedSolomon::new(data_shards, parity_shards).map(Arc::new))
            .clone()
    }
}

impl Default for ReedSolomonCache {
    fn default() -> Self {
        Self(RwLock::new(LruCache::new(Self::CAPACITY)))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::shred::{
            self, max_ticks_per_n_shreds, verify_test_data_shred, ShredType,
            CODING_SHREDS_PER_FEC_BLOCK,
        },
        assert_matches::assert_matches,
        itertools::Itertools,
        rand::Rng,
        solana_hash::Hash,
        solana_pubkey::Pubkey,
        solana_sha256_hasher::hash,
        solana_shred_version as shred_version,
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        std::{collections::HashSet, sync::Arc},
        test_case::test_matrix,
    };

    fn verify_test_code_shred(shred: &Shred, index: u32, slot: Slot, pk: &Pubkey, verify: bool) {
        assert_matches!(shred.sanitize(), Ok(()));
        assert!(!shred.is_data());
        assert_eq!(shred.index(), index);
        assert_eq!(shred.slot(), slot);
        assert_eq!(verify, shred.verify(pk));
    }

    fn run_test_data_shredder(slot: Slot, is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());

        // Test that parent cannot be > current slot
        assert_matches!(
            Shredder::new(slot, slot + 1, 0, 0),
            Err(Error::InvalidParentSlot { .. })
        );
        // Test that slot - parent cannot be > u16 MAX
        assert_matches!(
            Shredder::new(slot, slot - 1 - 0xffff, 0, 0),
            Err(Error::InvalidParentSlot { .. })
        );
        let parent_slot = slot - 5;
        let shredder = Shredder::new(slot, parent_slot, 0, 0).unwrap();
        let entries: Vec<_> = (0..5)
            .map(|_| {
                let keypair0 = Keypair::new();
                let keypair1 = Keypair::new();
                let tx0 =
                    system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
                Entry::new(&Hash::default(), 1, vec![tx0])
            })
            .collect();

        let num_expected_data_shreds = DATA_SHREDS_PER_FEC_BLOCK;
        let num_expected_coding_shreds = CODING_SHREDS_PER_FEC_BLOCK;
        let start_index = 0;
        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()), // chained_merkle_root
            start_index,                                // next_shred_index
            start_index,                                // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        let next_index = data_shreds.last().unwrap().index() + 1;
        assert_eq!(next_index as usize, num_expected_data_shreds);

        let mut data_shred_indexes = HashSet::new();
        let mut coding_shred_indexes = HashSet::new();
        for shred in data_shreds.iter() {
            assert_eq!(shred.shred_type(), ShredType::Data);
            let index = shred.index();
            let is_last = index as usize == num_expected_data_shreds - 1;
            verify_test_data_shred(
                shred,
                index,
                slot,
                parent_slot,
                &keypair.pubkey(),
                true, // verify
                is_last && is_last_in_slot,
                is_last,
            );
            assert!(!data_shred_indexes.contains(&index));
            data_shred_indexes.insert(index);
        }

        for shred in coding_shreds.iter() {
            let index = shred.index();
            assert_eq!(shred.shred_type(), ShredType::Code);
            verify_test_code_shred(shred, index, slot, &keypair.pubkey(), true);
            assert!(!coding_shred_indexes.contains(&index));
            coding_shred_indexes.insert(index);
        }

        for i in start_index..start_index + num_expected_data_shreds as u32 {
            assert!(data_shred_indexes.contains(&i));
        }

        for i in start_index..start_index + num_expected_coding_shreds as u32 {
            assert!(coding_shred_indexes.contains(&i));
        }

        assert_eq!(data_shred_indexes.len(), num_expected_data_shreds);
        assert_eq!(coding_shred_indexes.len(), num_expected_coding_shreds);

        // Test reassembly
        let deshred_payload = {
            let shreds = data_shreds.iter().map(Shred::payload);
            Shredder::deshred(shreds).unwrap()
        };
        let deshred_entries: Vec<Entry> = wincode::deserialize(&deshred_payload).unwrap();
        assert_eq!(entries, deshred_entries);
    }

    #[test_matrix([true, false])]
    fn test_data_shredder(is_last_in_slot: bool) {
        run_test_data_shredder(0x1234_5678_9abc_def0, is_last_in_slot);
    }

    #[test_matrix([true, false])]
    fn test_deserialize_shred_payload(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let shredder = Shredder::new(
            259_241_705, // slot
            259_241_698, // parent_slot
            178,         // reference_tick
            27_471,      // version
        )
        .unwrap();
        let entries: Vec<_> = (0..5)
            .map(|_| {
                let keypair0 = Keypair::new();
                let keypair1 = Keypair::new();
                let tx0 =
                    system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
                Entry::new(&Hash::default(), 1, vec![tx0])
            })
            .collect();

        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()), // chained_merkle_root
            369,                                        // next_shred_index
            776,                                        // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        for shred in [data_shreds, coding_shreds].into_iter().flatten() {
            let other = Shred::new_from_serialized_shred(shred.payload().clone());
            assert_eq!(shred, other.unwrap());
        }
    }

    #[test_matrix([true, false])]
    fn test_shred_reference_tick(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let slot = 1;
        let parent_slot = 0;
        let shredder = Shredder::new(slot, parent_slot, 5, 0).unwrap();
        let entries: Vec<_> = (0..5)
            .map(|_| {
                let keypair0 = Keypair::new();
                let keypair1 = Keypair::new();
                let tx0 =
                    system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
                Entry::new(&Hash::default(), 1, vec![tx0])
            })
            .collect();

        let (data_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()), // chained_merkle_root
            0,                                          // next_shred_index
            0,                                          // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        data_shreds.iter().for_each(|s| {
            assert_eq!(s.reference_tick(), 5);
            assert_eq!(shred::layout::get_reference_tick(s.payload()).unwrap(), 5);
        });

        let deserialized_shred =
            Shred::new_from_serialized_shred(data_shreds.last().unwrap().payload().clone())
                .unwrap();
        assert_eq!(deserialized_shred.reference_tick(), 5);
    }

    #[test_matrix([true, false])]
    fn test_shred_reference_tick_overflow(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let slot = 1;
        let parent_slot = 0;
        let shredder = Shredder::new(slot, parent_slot, u8::MAX, 0).unwrap();
        let entries: Vec<_> = (0..5)
            .map(|_| {
                let keypair0 = Keypair::new();
                let keypair1 = Keypair::new();
                let tx0 =
                    system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
                Entry::new(&Hash::default(), 1, vec![tx0])
            })
            .collect();

        let (data_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()), // chained_merkle_root
            0,                                          // next_shred_index
            0,                                          // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        data_shreds.iter().for_each(|s| {
            assert_eq!(
                s.reference_tick(),
                ShredFlags::SHRED_TICK_REFERENCE_MASK.bits()
            );
            assert_eq!(
                shred::layout::get_reference_tick(s.payload()).unwrap(),
                ShredFlags::SHRED_TICK_REFERENCE_MASK.bits()
            );
        });

        let deserialized_shred =
            Shred::new_from_serialized_shred(data_shreds.last().unwrap().payload().clone())
                .unwrap();
        assert_eq!(
            deserialized_shred.reference_tick(),
            ShredFlags::SHRED_TICK_REFERENCE_MASK.bits(),
        );
    }

    fn run_test_data_and_code_shredder(slot: Slot, is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let shredder = Shredder::new(slot, slot - 5, 0, 0).unwrap();
        // Create enough entries to make > 1 shred
        let data_buffer_size =
            ShredData::capacity(/*proof_size:*/ 6, /*resigned:*/ false).unwrap();
        let num_entries = max_ticks_per_n_shreds(1, Some(data_buffer_size)) + 1;
        let entries: Vec<_> = (0..num_entries)
            .map(|_| {
                let keypair0 = Keypair::new();
                let keypair1 = Keypair::new();
                let tx0 =
                    system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
                Entry::new(&Hash::default(), 1, vec![tx0])
            })
            .collect();

        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()), // chained_merkle_root
            0,                                          // next_shred_index
            0,                                          // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        for (i, s) in data_shreds.iter().enumerate() {
            verify_test_data_shred(
                s,
                s.index(),
                slot,
                slot - 5,
                &keypair.pubkey(),
                true,
                i == data_shreds.len() - 1 && is_last_in_slot,
                i == data_shreds.len() - 1,
            );
        }

        for s in coding_shreds {
            verify_test_code_shred(&s, s.index(), slot, &keypair.pubkey(), true);
        }
    }

    #[test_matrix([true, false])]
    fn test_data_and_code_shredder(is_last_in_slot: bool) {
        run_test_data_and_code_shredder(0x1234_5678_9abc_def0, is_last_in_slot);
    }

    #[test_matrix([true, false])]
    fn test_shred_version(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let hash = hash(Hash::default().as_ref());
        let version = shred_version::version_from_hash(&hash);
        assert_ne!(version, 0);
        let shredder = Shredder::new(0, 0, 0, version).unwrap();
        let entries: Vec<_> = (0..5)
            .map(|_| {
                let keypair0 = Keypair::new();
                let keypair1 = Keypair::new();
                let tx0 =
                    system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
                Entry::new(&Hash::default(), 1, vec![tx0])
            })
            .collect();

        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()), // chained_merkle_root
            0,                                          // next_shred_index
            0,                                          // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        assert!(!data_shreds
            .iter()
            .chain(coding_shreds.iter())
            .any(|s| s.version() != version));
    }

    #[test_matrix([true, false])]
    fn test_shred_fec_set_index(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let hash = hash(Hash::default().as_ref());
        let version = shred_version::version_from_hash(&hash);
        assert_ne!(version, 0);
        let shredder = Shredder::new(0, 0, 0, version).unwrap();
        let entries: Vec<_> = (0..500)
            .map(|_| {
                let keypair0 = Keypair::new();
                let keypair1 = Keypair::new();
                let tx0 =
                    system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
                Entry::new(&Hash::default(), 1, vec![tx0])
            })
            .collect();

        let start_index = 0x12;
        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()), // chained_merkle_root
            start_index,                                // next_shred_index
            start_index,                                // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        const MIN_CHUNK_SIZE: usize = DATA_SHREDS_PER_FEC_BLOCK;
        let chunks: Vec<_> = data_shreds
            .iter()
            .chunk_by(|shred| shred.fec_set_index())
            .into_iter()
            .map(|(fec_set_index, chunk)| (fec_set_index, chunk.count()))
            .collect();
        assert!(chunks
            .iter()
            .all(|(_, chunk_size)| *chunk_size >= MIN_CHUNK_SIZE));
        assert!(chunks
            .iter()
            .all(|(_, chunk_size)| *chunk_size < 2 * MIN_CHUNK_SIZE));
        assert_eq!(chunks[0].0, start_index);
        assert!(chunks.iter().tuple_windows().all(
            |((fec_set_index, chunk_size), (next_fec_set_index, _chunk_size))| fec_set_index
                + *chunk_size as u32
                == *next_fec_set_index
        ));
        assert!(coding_shreds.len() >= data_shreds.len());
        assert!(coding_shreds
            .iter()
            .zip(&data_shreds)
            .all(|(code, data)| code.fec_set_index() == data.fec_set_index()));
        assert_eq!(
            coding_shreds.last().unwrap().fec_set_index(),
            data_shreds.last().unwrap().fec_set_index()
        );
    }
}
