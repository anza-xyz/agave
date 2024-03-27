use {
    crate::{
        accounts_db::{AccountStorageEntry, PUBKEY_BINS_FOR_CALCULATING_HASHES},
        active_stats::{ActiveStatItem, ActiveStats},
        ancestors::Ancestors,
        pubkey_bins::PubkeyBinCalculator24,
    },
    blake3::OutputReader,
    bytemuck::{Pod, Zeroable},
    core::slice,
    log::*,
    memmap2::MmapMut,
    rayon::prelude::*,
    solana_measure::{measure::Measure, measure_us},
    solana_sdk::{
        bs58,
        hash::{Hash, Hasher},
        pubkey::Pubkey,
        rent_collector::RentCollector,
        slot_history::Slot,
        sysvar::epoch_schedule::EpochSchedule,
    },
    std::{
        borrow::Borrow,
        convert::TryInto,
        fmt,
        io::{Seek, SeekFrom, Write},
        mem,
        path::PathBuf,
        str::FromStr,
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc,
        },
        thread, time,
    },
    tempfile::tempfile_in,
    thiserror::Error,
};
pub const MERKLE_FANOUT: usize = 16;

/// 1 file containing account hashes sorted by pubkey, mapped into memory
struct MmapAccountHashesFile {
    /// raw slice of `Hash` values. Can be a larger slice than `count`
    mmap: MmapMut,
    /// # of valid Hash entries in `mmap`
    count: usize,
}

impl MmapAccountHashesFile {
    /// return a slice of account hashes starting at 'index'
    fn read(&self, index: usize) -> &[Hash] {
        let start = std::mem::size_of::<Hash>() * index;
        let end = std::mem::size_of::<Hash>() * self.count;
        let bytes = &self.mmap[start..end];
        bytemuck::cast_slice(bytes)
    }

    /// write a hash to the end of mmap file.
    fn write(&mut self, hash: &Hash) {
        let start = self.count * std::mem::size_of::<Hash>();
        let end = start + std::mem::size_of::<Hash>();
        self.mmap[start..end].copy_from_slice(hash.as_ref());
        self.count += 1;
    }
}

/// 1 file containing account hashes sorted by pubkey
struct AccountHashesFile {
    /// # hashes and an open file that will be deleted on drop. None if there are zero hashes to represent, and thus, no file.
    writer: Option<MmapAccountHashesFile>,
    /// The directory where temporary cache files are put
    dir_for_temp_cache_files: PathBuf,
    /// # bytes allocated
    capacity: usize,
}

impl AccountHashesFile {
    /// return a mmap reader that can be accessed  by slice
    fn get_reader(&mut self) -> Option<MmapAccountHashesFile> {
        std::mem::take(&mut self.writer)
    }

    /// # hashes stored in this file
    fn count(&self) -> usize {
        self.writer
            .as_ref()
            .map(|writer| writer.count)
            .unwrap_or_default()
    }

    /// write 'hash' to the file
    /// If the file isn't open, create it first.
    fn write(&mut self, hash: &Hash) {
        if self.writer.is_none() {
            // we have hashes to write but no file yet, so create a file that will auto-delete on drop

            let get_file = || -> Result<_, std::io::Error> {
                let mut data = tempfile_in(&self.dir_for_temp_cache_files).unwrap_or_else(|err| {
                    panic!(
                        "Unable to create file within {}: {err}",
                        self.dir_for_temp_cache_files.display()
                    )
                });

                // Theoretical performance optimization: write a zero to the end of
                // the file so that we won't have to resize it later, which may be
                // expensive.
                assert!(self.capacity > 0);
                data.seek(SeekFrom::Start((self.capacity - 1) as u64))?;
                data.write_all(&[0])?;
                data.rewind()?;
                data.flush()?;
                Ok(data)
            };

            // Retry 5 times to allocate the AccountHashesFile. The memory might be fragmented and
            // causes memory allocation failure. Therefore, let's retry after failure. Hoping that the
            // kernel has the chance to defrag the memory between the retries, and retries succeed.
            let mut num_retries = 0;
            let data = loop {
                num_retries += 1;

                match get_file() {
                    Ok(data) => {
                        break data;
                    }
                    Err(err) => {
                        info!(
                            "Unable to create account hashes file within {}: {}, retry counter {}",
                            self.dir_for_temp_cache_files.display(),
                            err,
                            num_retries
                        );

                        if num_retries > 5 {
                            panic!(
                                "Unable to create account hashes file within {}: after {} retries",
                                self.dir_for_temp_cache_files.display(),
                                num_retries
                            );
                        }
                        datapoint_info!(
                            "retry_account_hashes_file_allocation",
                            ("retry", num_retries, i64)
                        );
                        thread::sleep(time::Duration::from_millis(num_retries * 100));
                    }
                }
            };

            //UNSAFE: Required to create a Mmap
            let map = unsafe { MmapMut::map_mut(&data) };
            let map = map.unwrap_or_else(|e| {
                error!(
                    "Failed to map the data file (size: {}): {}.\n
                        Please increase sysctl vm.max_map_count or equivalent for your platform.",
                    self.capacity, e
                );
                std::process::exit(1);
            });

            self.writer = Some(MmapAccountHashesFile {
                mmap: map,
                count: 0,
            });
        }
        self.writer.as_mut().unwrap().write(hash);
    }
}

/// parameters to calculate accounts hash
#[derive(Debug)]
pub struct CalcAccountsHashConfig<'a> {
    /// true to use a thread pool dedicated to bg operations
    pub use_bg_thread_pool: bool,
    /// verify every hash in append vec/write cache with a recalculated hash
    pub check_hash: bool,
    /// 'ancestors' is used to get storages
    pub ancestors: Option<&'a Ancestors>,
    /// does hash calc need to consider account data that exists in the write cache?
    /// if so, 'ancestors' will be used for this purpose as well as storages.
    pub epoch_schedule: &'a EpochSchedule,
    pub rent_collector: &'a RentCollector,
    /// used for tracking down hash mismatches after the fact
    pub store_detailed_debug_info_on_failure: bool,
}

// smallest, 3 quartiles, largest, average
pub type StorageSizeQuartileStats = [usize; 6];

#[derive(Debug, Default)]
pub struct HashStats {
    pub total_us: u64,
    pub mark_time_us: u64,
    pub cache_hash_data_us: u64,
    pub scan_time_total_us: u64,
    pub zeros_time_total_us: u64,
    pub hash_time_total_us: u64,
    pub sort_time_total_us: u64,
    pub hash_total: usize,
    pub num_snapshot_storage: usize,
    pub scan_chunks: usize,
    pub num_slots: usize,
    pub num_dirty_slots: usize,
    pub collect_snapshots_us: u64,
    pub storage_sort_us: u64,
    pub storage_size_quartiles: StorageSizeQuartileStats,
    pub oldest_root: Slot,
    pub roots_older_than_epoch: AtomicUsize,
    pub accounts_in_roots_older_than_epoch: AtomicUsize,
    pub append_vec_sizes_older_than_epoch: AtomicUsize,
    pub longest_ancient_scan_us: AtomicU64,
    pub sum_ancient_scans_us: AtomicU64,
    pub count_ancient_scans: AtomicU64,
    pub pubkey_bin_search_us: AtomicU64,
}
impl HashStats {
    pub fn calc_storage_size_quartiles(&mut self, storages: &[Arc<AccountStorageEntry>]) {
        let mut sum = 0;
        let mut sizes = storages
            .iter()
            .map(|storage| {
                let cap = storage.accounts.capacity() as usize;
                sum += cap;
                cap
            })
            .collect::<Vec<_>>();
        sizes.sort_unstable();
        let len = sizes.len();
        self.storage_size_quartiles = if len == 0 {
            StorageSizeQuartileStats::default()
        } else {
            [
                *sizes.first().unwrap(),
                sizes[len / 4],
                sizes[len * 2 / 4],
                sizes[len * 3 / 4],
                *sizes.last().unwrap(),
                sum / len,
            ]
        };
    }

    pub fn log(&self) {
        datapoint_info!(
            "calculate_accounts_hash_from_storages",
            ("total_us", self.total_us, i64),
            ("mark_time_us", self.mark_time_us, i64),
            ("cache_hash_data_us", self.cache_hash_data_us, i64),
            ("accounts_scan_us", self.scan_time_total_us, i64),
            ("eliminate_zeros_us", self.zeros_time_total_us, i64),
            ("hash_us", self.hash_time_total_us, i64),
            ("sort_us", self.sort_time_total_us, i64),
            ("hash_total", self.hash_total, i64),
            ("storage_sort_us", self.storage_sort_us, i64),
            ("collect_snapshots_us", self.collect_snapshots_us, i64),
            ("num_snapshot_storage", self.num_snapshot_storage, i64),
            ("scan_chunks", self.scan_chunks, i64),
            ("num_slots", self.num_slots, i64),
            ("num_dirty_slots", self.num_dirty_slots, i64),
            ("storage_size_min", self.storage_size_quartiles[0], i64),
            (
                "storage_size_quartile_1",
                self.storage_size_quartiles[1],
                i64
            ),
            (
                "storage_size_quartile_2",
                self.storage_size_quartiles[2],
                i64
            ),
            (
                "storage_size_quartile_3",
                self.storage_size_quartiles[3],
                i64
            ),
            ("storage_size_max", self.storage_size_quartiles[4], i64),
            ("storage_size_avg", self.storage_size_quartiles[5], i64),
            (
                "roots_older_than_epoch",
                self.roots_older_than_epoch.load(Ordering::Relaxed),
                i64
            ),
            ("oldest_root", self.oldest_root, i64),
            (
                "longest_ancient_scan_us",
                self.longest_ancient_scan_us.load(Ordering::Relaxed),
                i64
            ),
            (
                "sum_ancient_scans_us",
                self.sum_ancient_scans_us.load(Ordering::Relaxed),
                i64
            ),
            (
                "count_ancient_scans",
                self.count_ancient_scans.load(Ordering::Relaxed),
                i64
            ),
            (
                "append_vec_sizes_older_than_epoch",
                self.append_vec_sizes_older_than_epoch
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "accounts_in_roots_older_than_epoch",
                self.accounts_in_roots_older_than_epoch
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "pubkey_bin_search_us",
                self.pubkey_bin_search_us.load(Ordering::Relaxed),
                i64
            ),
        );
    }
}

/// While scanning appendvecs, this is the info that needs to be extracted, de-duped, and sorted from what is stored in an append vec.
/// Note this can be saved/loaded during hash calculation to a memory mapped file whose contents are
/// [CalculateHashIntermediate]
#[repr(C)]
#[derive(Debug, PartialEq, Eq, Clone, Copy, Pod, Zeroable)]
pub struct CalculateHashIntermediate {
    pub hash: AccountHash,
    pub lamports: u64,
    pub pubkey: Pubkey,
}

// In order to safely guarantee CalculateHashIntermediate is Pod, it cannot have any padding
const _: () = assert!(
    std::mem::size_of::<CalculateHashIntermediate>()
        == std::mem::size_of::<AccountHash>()
            + std::mem::size_of::<u64>()
            + std::mem::size_of::<Pubkey>(),
    "CalculateHashIntermediate cannot have any padding"
);

#[derive(Debug, PartialEq, Eq)]
struct CumulativeOffset {
    /// Since the source data is at most 2D, two indexes are enough.
    index: [usize; 2],
    start_offset: usize,
}

trait ExtractSliceFromRawData<'b, T: 'b> {
    fn extract<'a>(&'b self, offset: &'a CumulativeOffset, start: usize) -> &'b [T];
}

impl<'b, T: 'b> ExtractSliceFromRawData<'b, T> for Vec<Vec<T>> {
    fn extract<'a>(&'b self, offset: &'a CumulativeOffset, start: usize) -> &'b [T] {
        &self[offset.index[0]][start..]
    }
}

impl<'b, T: 'b> ExtractSliceFromRawData<'b, T> for Vec<Vec<Vec<T>>> {
    fn extract<'a>(&'b self, offset: &'a CumulativeOffset, start: usize) -> &'b [T] {
        &self[offset.index[0]][offset.index[1]][start..]
    }
}

// Allow retrieving &[start..end] from a logical src: Vec<T>, where src is really Vec<Vec<T>> (or later Vec<Vec<Vec<T>>>)
// This model prevents callers from having to flatten which saves both working memory and time.
#[derive(Default, Debug)]
struct CumulativeOffsets {
    cumulative_offsets: Vec<CumulativeOffset>,
    total_count: usize,
}

/// used by merkle tree calculation to lookup account hashes by overall index
#[derive(Default)]
struct CumulativeHashesFromFiles {
    /// source of hashes in order
    readers: Vec<MmapAccountHashesFile>,
    /// look up reader index and offset by overall index
    cumulative: CumulativeOffsets,
}

impl CumulativeHashesFromFiles {
    /// Calculate offset from overall index to which file and offset within that file based on the length of each hash file.
    /// Also collect readers to access the data.
    fn from_files(hashes: Vec<AccountHashesFile>) -> Self {
        let mut readers = Vec::with_capacity(hashes.len());
        let cumulative = CumulativeOffsets::new(hashes.into_iter().filter_map(|mut hash_file| {
            // ignores all hashfiles that have zero entries
            hash_file.get_reader().map(|reader| {
                let count = reader.count;
                readers.push(reader);
                count
            })
        }));
        Self {
            cumulative,
            readers,
        }
    }

    /// total # of items referenced
    fn total_count(&self) -> usize {
        self.cumulative.total_count
    }

    // return the biggest slice possible that starts at the overall index 'start'
    fn get_slice(&self, start: usize) -> &[Hash] {
        let (start, offset) = self.cumulative.find(start);
        let data_source_index = offset.index[0];
        let data = &self.readers[data_source_index];
        // unwrap here because we should never ask for data that doesn't exist. If we do, then cumulative calculated incorrectly.
        data.read(start)
    }
}

impl CumulativeOffsets {
    fn new<I>(iter: I) -> Self
    where
        I: Iterator<Item = usize>,
    {
        let mut total_count: usize = 0;
        let cumulative_offsets: Vec<_> = iter
            .enumerate()
            .filter_map(|(i, len)| {
                if len > 0 {
                    let result = CumulativeOffset {
                        index: [i, i],
                        start_offset: total_count,
                    };
                    total_count += len;
                    Some(result)
                } else {
                    None
                }
            })
            .collect();

        Self {
            cumulative_offsets,
            total_count,
        }
    }

    fn from_raw<T>(raw: &[Vec<T>]) -> Self {
        Self::new(raw.iter().map(|v| v.len()))
    }

    /// find the index of the data source that contains 'start'
    fn find_index(&self, start: usize) -> usize {
        assert!(!self.cumulative_offsets.is_empty());
        match self.cumulative_offsets[..].binary_search_by(|index| index.start_offset.cmp(&start)) {
            Ok(index) => index,
            Err(index) => index - 1, // we would insert at index so we are before the item at index
        }
    }

    /// given overall start index 'start'
    /// return ('start', which is the offset into the data source at 'index',
    ///     and 'index', which is the data source to use)
    fn find(&self, start: usize) -> (usize, &CumulativeOffset) {
        let index = self.find_index(start);
        let index = &self.cumulative_offsets[index];
        let start = start - index.start_offset;
        (start, index)
    }

    // return the biggest slice possible that starts at 'start'
    fn get_slice<'a, 'b, T, U>(&'a self, raw: &'b U, start: usize) -> &'b [T]
    where
        U: ExtractSliceFromRawData<'b, T> + 'b,
    {
        let (start, index) = self.find(start);
        raw.extract(index, start)
    }
}

#[derive(Debug)]
pub struct AccountsHasher<'a> {
    pub zero_lamport_accounts: ZeroLamportAccounts,
    /// The directory where temporary cache files are put
    pub dir_for_temp_cache_files: PathBuf,
    pub(crate) active_stats: &'a ActiveStats,
}

/// Pointer to a specific item in chunked accounts hash slices.
#[derive(Debug, Clone, Copy)]
struct SlotGroupPointer {
    /// slot group index
    slot_group_index: usize,
    /// offset within a slot group
    offset: usize,
}

/// A struct for the location of an account hash item inside chunked accounts hash slices.
#[derive(Debug)]
struct ItemLocation<'a> {
    /// account's pubkey
    key: &'a Pubkey,
    /// pointer to the item in slot group slices
    pointer: SlotGroupPointer,
}

impl<'a> AccountsHasher<'a> {
    pub fn calculate_hash(hashes: Vec<Vec<Hash>>) -> (Hash, usize) {
        let cumulative_offsets = CumulativeOffsets::from_raw(&hashes);

        let hash_total = cumulative_offsets.total_count;
        let result = AccountsHasher::compute_merkle_root_from_slices(
            hash_total,
            MERKLE_FANOUT,
            None,
            |start: usize| cumulative_offsets.get_slice(&hashes, start),
            None,
        );
        (result.0, hash_total)
    }

    pub fn compute_merkle_root(hashes: Vec<(Pubkey, Hash)>, fanout: usize) -> Hash {
        Self::compute_merkle_root_loop(hashes, fanout, |t| &t.1)
    }

    // this function avoids an infinite recursion compiler error
    pub fn compute_merkle_root_recurse(hashes: Vec<Hash>, fanout: usize) -> Hash {
        Self::compute_merkle_root_loop(hashes, fanout, |t| t)
    }

    pub fn div_ceil(x: usize, y: usize) -> usize {
        let mut result = x / y;
        if x % y != 0 {
            result += 1;
        }
        result
    }

    // For the first iteration, there could be more items in the tuple than just hash and lamports.
    // Using extractor allows us to avoid an unnecessary array copy on the first iteration.
    pub fn compute_merkle_root_loop<T, F>(hashes: Vec<T>, fanout: usize, extractor: F) -> Hash
    where
        F: Fn(&T) -> &Hash + std::marker::Sync,
        T: std::marker::Sync,
    {
        if hashes.is_empty() {
            return Hasher::default().result();
        }

        let mut time = Measure::start("time");

        let total_hashes = hashes.len();
        let chunks = Self::div_ceil(total_hashes, fanout);

        let result: Vec<_> = (0..chunks)
            .into_par_iter()
            .map(|i| {
                let start_index = i * fanout;
                let end_index = std::cmp::min(start_index + fanout, total_hashes);

                let mut hasher = Hasher::default();
                for item in hashes.iter().take(end_index).skip(start_index) {
                    let h = extractor(item);
                    hasher.hash(h.as_ref());
                }

                hasher.result()
            })
            .collect();
        time.stop();
        debug!("hashing {} {}", total_hashes, time);

        if result.len() == 1 {
            result[0]
        } else {
            Self::compute_merkle_root_recurse(result, fanout)
        }
    }

    fn calculate_three_level_chunks(
        total_hashes: usize,
        fanout: usize,
        max_levels_per_pass: Option<usize>,
        specific_level_count: Option<usize>,
    ) -> (usize, usize, bool) {
        const THREE_LEVEL_OPTIMIZATION: usize = 3; // this '3' is dependent on the code structure below where we manually unroll
        let target = fanout.pow(THREE_LEVEL_OPTIMIZATION as u32);

        // Only use the 3 level optimization if we have at least 4 levels of data.
        // Otherwise, we'll be serializing a parallel operation.
        let threshold = target * fanout;
        let mut three_level = max_levels_per_pass.unwrap_or(usize::MAX) >= THREE_LEVEL_OPTIMIZATION
            && total_hashes >= threshold;
        if three_level {
            if let Some(specific_level_count_value) = specific_level_count {
                three_level = specific_level_count_value >= THREE_LEVEL_OPTIMIZATION;
            }
        }
        let (num_hashes_per_chunk, levels_hashed) = if three_level {
            (target, THREE_LEVEL_OPTIMIZATION)
        } else {
            (fanout, 1)
        };
        (num_hashes_per_chunk, levels_hashed, three_level)
    }

    // This function is designed to allow hashes to be located in multiple, perhaps multiply deep vecs.
    // The caller provides a function to return a slice from the source data.
    fn compute_merkle_root_from_slices<'b, F, T>(
        total_hashes: usize,
        fanout: usize,
        max_levels_per_pass: Option<usize>,
        get_hash_slice_starting_at_index: F,
        specific_level_count: Option<usize>,
    ) -> (Hash, Vec<Hash>)
    where
        // returns a slice of hashes starting at the given overall index
        F: Fn(usize) -> &'b [T] + std::marker::Sync,
        T: Borrow<Hash> + std::marker::Sync + 'b,
    {
        if total_hashes == 0 {
            return (Hasher::default().result(), vec![]);
        }

        let mut time = Measure::start("time");

        let (num_hashes_per_chunk, levels_hashed, three_level) = Self::calculate_three_level_chunks(
            total_hashes,
            fanout,
            max_levels_per_pass,
            specific_level_count,
        );

        let chunks = Self::div_ceil(total_hashes, num_hashes_per_chunk);

        // initial fetch - could return entire slice
        let data = get_hash_slice_starting_at_index(0);
        let data_len = data.len();

        let result: Vec<_> = (0..chunks)
            .into_par_iter()
            .map(|i| {
                // summary:
                // this closure computes 1 or 3 levels of merkle tree (all chunks will be 1 or all will be 3)
                // for a subset (our chunk) of the input data [start_index..end_index]

                // index into get_hash_slice_starting_at_index where this chunk's range begins
                let start_index = i * num_hashes_per_chunk;
                // index into get_hash_slice_starting_at_index where this chunk's range ends
                let end_index = std::cmp::min(start_index + num_hashes_per_chunk, total_hashes);

                // will compute the final result for this closure
                let mut hasher = Hasher::default();

                // index into 'data' where we are currently pulling data
                // if we exhaust our data, then we will request a new slice, and data_index resets to 0, the beginning of the new slice
                let mut data_index = start_index;
                // source data, which we may refresh when we exhaust
                let mut data = data;
                // len of the source data
                let mut data_len = data_len;

                if !three_level {
                    // 1 group of fanout
                    // The result of this loop is a single hash value from fanout input hashes.
                    for i in start_index..end_index {
                        if data_index >= data_len {
                            // we exhausted our data, fetch next slice starting at i
                            data = get_hash_slice_starting_at_index(i);
                            data_len = data.len();
                            data_index = 0;
                        }
                        hasher.hash(data[data_index].borrow().as_ref());
                        data_index += 1;
                    }
                } else {
                    // hash 3 levels of fanout simultaneously.
                    // This codepath produces 1 hash value for between 1..=fanout^3 input hashes.
                    // It is equivalent to running the normal merkle tree calculation 3 iterations on the input.
                    //
                    // big idea:
                    //  merkle trees usually reduce the input vector by a factor of fanout with each iteration
                    //  example with fanout 2:
                    //   start:     [0,1,2,3,4,5,6,7]      in our case: [...16M...] or really, 1B
                    //   iteration0 [.5, 2.5, 4.5, 6.5]                 [... 1M...]
                    //   iteration1 [1.5, 5.5]                          [...65k...]
                    //   iteration2 3.5                                 [...4k... ]
                    //  So iteration 0 consumes N elements, hashes them in groups of 'fanout' and produces a vector of N/fanout elements
                    //   and the process repeats until there is only 1 hash left.
                    //
                    //  With the three_level code path, we make each chunk we iterate of size fanout^3 (4096)
                    //  So, the input could be 16M hashes and the output will be 4k hashes, or N/fanout^3
                    //  The goal is to reduce the amount of data that has to be constructed and held in memory.
                    //  When we know we have enough hashes, then, in 1 pass, we hash 3 levels simultaneously, storing far fewer intermediate hashes.
                    //
                    // Now, some details:
                    // The result of this loop is a single hash value from fanout^3 input hashes.
                    // concepts:
                    //  what we're conceptually hashing: "raw_hashes"[start_index..end_index]
                    //   example: [a,b,c,d,e,f]
                    //   but... hashes[] may really be multiple vectors that are pieced together.
                    //   example: [[a,b],[c],[d,e,f]]
                    //   get_hash_slice_starting_at_index(any_index) abstracts that and returns a slice starting at raw_hashes[any_index..]
                    //   such that the end of get_hash_slice_starting_at_index may be <, >, or = end_index
                    //   example: get_hash_slice_starting_at_index(1) returns [b]
                    //            get_hash_slice_starting_at_index(3) returns [d,e,f]
                    // This code is basically 3 iterations of merkle tree hashing occurring simultaneously.
                    // The first fanout raw hashes are hashed in hasher_k. This is iteration0
                    // Once hasher_k has hashed fanout hashes, hasher_k's result hash is hashed in hasher_j and then discarded
                    // hasher_k then starts over fresh and hashes the next fanout raw hashes. This is iteration0 again for a new set of data.
                    // Once hasher_j has hashed fanout hashes (from k), hasher_j's result hash is hashed in hasher and then discarded
                    // Once hasher has hashed fanout hashes (from j), then the result of hasher is the hash for fanout^3 raw hashes.
                    // If there are < fanout^3 hashes, then this code stops when it runs out of raw hashes and returns whatever it hashed.
                    // This is always how the very last elements work in a merkle tree.
                    let mut i = start_index;
                    while i < end_index {
                        let mut hasher_j = Hasher::default();
                        for _j in 0..fanout {
                            let mut hasher_k = Hasher::default();
                            let end = std::cmp::min(end_index - i, fanout);
                            for _k in 0..end {
                                if data_index >= data_len {
                                    // we exhausted our data, fetch next slice starting at i
                                    data = get_hash_slice_starting_at_index(i);
                                    data_len = data.len();
                                    data_index = 0;
                                }
                                hasher_k.hash(data[data_index].borrow().as_ref());
                                data_index += 1;
                                i += 1;
                            }
                            hasher_j.hash(hasher_k.result().as_ref());
                            if i >= end_index {
                                break;
                            }
                        }
                        hasher.hash(hasher_j.result().as_ref());
                    }
                }

                hasher.result()
            })
            .collect();
        time.stop();
        debug!("hashing {} {}", total_hashes, time);

        if let Some(mut specific_level_count_value) = specific_level_count {
            specific_level_count_value -= levels_hashed;
            if specific_level_count_value == 0 {
                (Hash::default(), result)
            } else {
                assert!(specific_level_count_value > 0);
                // We did not hash the number of levels required by 'specific_level_count', so repeat
                Self::compute_merkle_root_from_slices_recurse(
                    result,
                    fanout,
                    max_levels_per_pass,
                    Some(specific_level_count_value),
                )
            }
        } else {
            (
                if result.len() == 1 {
                    result[0]
                } else {
                    Self::compute_merkle_root_recurse(result, fanout)
                },
                vec![], // no intermediate results needed by caller
            )
        }
    }

    fn compute_merkle_root_from_slices_recurse(
        hashes: Vec<Hash>,
        fanout: usize,
        max_levels_per_pass: Option<usize>,
        specific_level_count: Option<usize>,
    ) -> (Hash, Vec<Hash>) {
        Self::compute_merkle_root_from_slices(
            hashes.len(),
            fanout,
            max_levels_per_pass,
            |start| &hashes[start..],
            specific_level_count,
        )
    }

    pub fn accumulate_account_hashes(mut hashes: Vec<(Pubkey, AccountHash)>) -> Hash {
        hashes.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Self::compute_merkle_root_loop(hashes, MERKLE_FANOUT, |i| &i.1 .0)
    }

    pub fn compare_two_hash_entries(
        a: &CalculateHashIntermediate,
        b: &CalculateHashIntermediate,
    ) -> std::cmp::Ordering {
        // note partial_cmp only returns None with floating point comparisons
        a.pubkey.partial_cmp(&b.pubkey).unwrap()
    }

    pub fn checked_cast_for_capitalization(balance: u128) -> u64 {
        balance.try_into().unwrap_or_else(|_| {
            panic!("overflow is detected while summing capitalization: {balance}")
        })
    }

    /// returns:
    /// Vec, with one entry per bin
    ///  for each entry, Vec<Hash> in pubkey order
    /// If return Vec<AccountHashesFile> was flattened, it would be all hashes, in pubkey order.
    fn de_dup_accounts(
        &self,
        sorted_data_by_pubkey: &[&[CalculateHashIntermediate]],
        stats: &mut HashStats,
        max_bin: usize,
    ) -> (Vec<AccountHashesFile>, u64) {
        // 1. eliminate zero lamport accounts
        // 2. pick the highest slot or (slot = and highest version) of each pubkey
        // 3. produce this output:
        // a. vec: PUBKEY_BINS_FOR_CALCULATING_HASHES in pubkey order
        //      vec: individual hashes in pubkey order, 1 hash per
        // b. lamports
        let _guard = self.active_stats.activate(ActiveStatItem::HashDeDup);

        #[derive(Default)]
        struct DedupResult {
            hashes_files: Vec<AccountHashesFile>,
            hashes_count: usize,
            lamports_sum: u64,
        }

        let mut zeros = Measure::start("eliminate zeros");
        let DedupResult {
            hashes_files: hashes,
            hashes_count: hash_total,
            lamports_sum: lamports_total,
        } = (0..max_bin)
            .into_par_iter()
            .fold(DedupResult::default, |mut accum, bin| {
                let (hashes_file, lamports_bin) =
                    self.de_dup_accounts_in_parallel(sorted_data_by_pubkey, bin, max_bin, stats);

                accum.lamports_sum = accum
                    .lamports_sum
                    .checked_add(lamports_bin)
                    .expect("summing capitalization cannot overflow");
                accum.hashes_count += hashes_file.count();
                accum.hashes_files.push(hashes_file);
                accum
            })
            .reduce(
                || DedupResult {
                    hashes_files: Vec::with_capacity(max_bin),
                    ..Default::default()
                },
                |mut a, mut b| {
                    a.lamports_sum = a
                        .lamports_sum
                        .checked_add(b.lamports_sum)
                        .expect("summing capitalization cannot overflow");
                    a.hashes_count += b.hashes_count;
                    a.hashes_files.append(&mut b.hashes_files);
                    a
                },
            );
        zeros.stop();
        stats.zeros_time_total_us += zeros.as_us();
        stats.hash_total += hash_total;
        (hashes, lamports_total)
    }

    /// Given the item location, return the item in the `CalculatedHashIntermediate` slices and the next item location in the same bin.
    /// If the end of the `CalculatedHashIntermediate` slice is reached or all the accounts in current bin have been exhausted, return `None` for next item location.
    fn get_item<'b>(
        sorted_data_by_pubkey: &[&'b [CalculateHashIntermediate]],
        bin: usize,
        binner: &PubkeyBinCalculator24,
        item_loc: &ItemLocation<'b>,
    ) -> (&'b CalculateHashIntermediate, Option<ItemLocation<'b>>) {
        let division_data = &sorted_data_by_pubkey[item_loc.pointer.slot_group_index];
        let mut index = item_loc.pointer.offset;
        index += 1;
        let mut next = None;

        while index < division_data.len() {
            // still more items where we found the previous key, so just increment the index for that slot group, skipping all pubkeys that are equal
            let next_key = &division_data[index].pubkey;
            if next_key == item_loc.key {
                index += 1;
                continue; // duplicate entries of same pubkey, so keep skipping
            }

            if binner.bin_from_pubkey(next_key) > bin {
                // the next pubkey is not in our bin
                break;
            }

            // point to the next pubkey > key
            next = Some(ItemLocation {
                key: next_key,
                pointer: SlotGroupPointer {
                    slot_group_index: item_loc.pointer.slot_group_index,
                    offset: index,
                },
            });
            break;
        }

        // this is the previous first item that was requested
        (&division_data[index - 1], next)
    }

    /// `hash_data` must be sorted by `binner.bin_from_pubkey()`
    /// return index in `hash_data` of first pubkey that is in `bin`, based on `binner`
    fn binary_search_for_first_pubkey_in_bin(
        hash_data: &[CalculateHashIntermediate],
        bin: usize,
        binner: &PubkeyBinCalculator24,
    ) -> Option<usize> {
        let potential_index = if bin == 0 {
            // `bin` == 0 is special because there cannot be `bin`-1
            // so either element[0] is in bin 0 or there is nothing in bin 0.
            0
        } else {
            // search for the first pubkey that is in `bin`
            // There could be many keys in a row with the same `bin`.
            // So, for each pubkey, use calculated_bin * 2 + 1 as the bin of a given pubkey for binary search.
            // And compare the bin of each pubkey with `bin` * 2.
            // So all keys that are in `bin` will compare as `bin` * 2 + 1
            // all keys that are in `bin`-1 will compare as ((`bin` - 1) * 2 + 1), which is (`bin` * 2 - 1)
            // NO keys will compare as `bin` * 2 because we add 1.
            // So, the binary search will NEVER return Ok(found_index), but will always return Err(index of first key in `bin`).
            // Note that if NO key is in `bin`, then the key at the found index will be in a bin > `bin`, so return None.
            let just_prior_to_desired_bin = bin * 2;
            let search = hash_data.binary_search_by(|data| {
                (1 + 2 * binner.bin_from_pubkey(&data.pubkey)).cmp(&just_prior_to_desired_bin)
            });
            // returns Err(index where item should be) since the desired item will never exist
            search.expect_err("it is impossible to find a matching bin")
        };
        // note that `potential_index` could be == hash_data.len(). This indicates the first key in `bin` would be
        // after the data we have. Thus, no key is in `bin`.
        // This also handles the case where `hash_data` is empty, since len() will be 0 and `get` will return None.
        hash_data.get(potential_index).and_then(|potential_data| {
            (binner.bin_from_pubkey(&potential_data.pubkey) == bin).then_some(potential_index)
        })
    }

    /// `hash_data` must be sorted by `binner.bin_from_pubkey()`
    /// return index in `hash_data` of first pubkey that is in `bin`, based on `binner`
    fn find_first_pubkey_in_bin(
        hash_data: &[CalculateHashIntermediate],
        bin: usize,
        bins: usize,
        binner: &PubkeyBinCalculator24,
        stats: &HashStats,
    ) -> Option<usize> {
        if hash_data.is_empty() {
            return None;
        }
        let (result, us) = measure_us!({
            // assume uniform distribution of pubkeys and choose first guess based on bin we're looking for
            let i = hash_data.len() * bin / bins;
            let estimate = &hash_data[i];

            let pubkey_bin = binner.bin_from_pubkey(&estimate.pubkey);
            let range = if pubkey_bin >= bin {
                // i pubkey matches or is too large, so look <= i for the first pubkey in the right bin
                // i+1 could be the first pubkey in the right bin
                0..(i + 1)
            } else {
                // i pubkey is too small, so look after i
                (i + 1)..hash_data.len()
            };
            Some(
                range.start +
                // binary search the subset
                Self::binary_search_for_first_pubkey_in_bin(
                    &hash_data[range],
                    bin,
                    binner,
                )?,
            )
        });
        stats.pubkey_bin_search_us.fetch_add(us, Ordering::Relaxed);
        result
    }

    /// Return the working_set and max number of pubkeys for hash dedup.
    /// `working_set` holds SlotGroupPointer {slot_group_index, offset} for items in account's pubkey descending order.
    fn initialize_dedup_working_set(
        sorted_data_by_pubkey: &[&[CalculateHashIntermediate]],
        pubkey_bin: usize,
        bins: usize,
        binner: &PubkeyBinCalculator24,
        stats: &HashStats,
    ) -> (
        Vec<SlotGroupPointer>, /* working_set */
        usize,                 /* max_inclusive_num_pubkeys */
    ) {
        // working_set holds the lowest items for each slot_group sorted by pubkey descending (min_key is the last)
        let mut working_set: Vec<SlotGroupPointer> = Vec::default();

        // Initialize 'working_set', which holds the current lowest item in each slot group.
        // `working_set` should be initialized in reverse order of slot_groups. Later slot_groups are
        // processed first. For each slot_group, if the lowest item for current slot group is
        // already in working_set (i.e. inserted by a later slot group), the next lowest item
        // in this slot group is searched and checked, until either one that is `not` in the
        // working_set is found, which will then be inserted, or no next lowest item is found.
        // Iterating in reverse order of slot_group will guarantee that each slot group will be
        // scanned only once and scanned continuously. Therefore, it can achieve better data
        // locality during the scan.
        let max_inclusive_num_pubkeys = sorted_data_by_pubkey
            .iter()
            .enumerate()
            .rev()
            .map(|(i, hash_data)| {
                let first_pubkey_in_bin =
                    Self::find_first_pubkey_in_bin(hash_data, pubkey_bin, bins, binner, stats);

                if let Some(first_pubkey_in_bin) = first_pubkey_in_bin {
                    let mut next = Some(ItemLocation {
                        key: &hash_data[first_pubkey_in_bin].pubkey,
                        pointer: SlotGroupPointer {
                            slot_group_index: i,
                            offset: first_pubkey_in_bin,
                        },
                    });

                    Self::add_next_item(
                        &mut next,
                        &mut working_set,
                        sorted_data_by_pubkey,
                        pubkey_bin,
                        binner,
                    );

                    let mut first_pubkey_in_next_bin = first_pubkey_in_bin + 1;
                    while first_pubkey_in_next_bin < hash_data.len() {
                        if binner.bin_from_pubkey(&hash_data[first_pubkey_in_next_bin].pubkey)
                            != pubkey_bin
                        {
                            break;
                        }
                        first_pubkey_in_next_bin += 1;
                    }
                    first_pubkey_in_next_bin - first_pubkey_in_bin
                } else {
                    0
                }
            })
            .sum::<usize>();

        (working_set, max_inclusive_num_pubkeys)
    }

    /// Add next item into hash dedup working set
    fn add_next_item<'b>(
        next: &mut Option<ItemLocation<'b>>,
        working_set: &mut Vec<SlotGroupPointer>,
        sorted_data_by_pubkey: &[&'b [CalculateHashIntermediate]],
        pubkey_bin: usize,
        binner: &PubkeyBinCalculator24,
    ) {
        // looping to add next item to working set
        while let Some(ItemLocation { key, pointer }) = std::mem::take(next) {
            // if `new key` is less than the min key in the working set, skip binary search and
            // insert item to the end vec directly
            if let Some(SlotGroupPointer {
                slot_group_index: current_min_slot_group_index,
                offset: current_min_offset,
            }) = working_set.last()
            {
                let current_min_key = &sorted_data_by_pubkey[*current_min_slot_group_index]
                    [*current_min_offset]
                    .pubkey;
                if key < current_min_key {
                    working_set.push(pointer);
                    break;
                }
            }

            let found = working_set.binary_search_by(|pointer| {
                let prob = &sorted_data_by_pubkey[pointer.slot_group_index][pointer.offset].pubkey;
                (*key).cmp(prob)
            });

            match found {
                Err(index) => {
                    // found a new new key, insert into the working_set. This is O(n/2) on
                    // average. Theoretically, this operation could be expensive and may be further
                    // optimized in future.
                    working_set.insert(index, pointer);
                    break;
                }
                Ok(index) => {
                    let found = &mut working_set[index];
                    if found.slot_group_index > pointer.slot_group_index {
                        // There is already a later slot group that contains this key in the working_set,
                        // look up again.
                        let (_item, new_next) = Self::get_item(
                            sorted_data_by_pubkey,
                            pubkey_bin,
                            binner,
                            &ItemLocation { key, pointer },
                        );
                        *next = new_next;
                    } else {
                        // A previous slot contains this key, replace it, and look for next item in the previous slot group.
                        let (_item, new_next) = Self::get_item(
                            sorted_data_by_pubkey,
                            pubkey_bin,
                            binner,
                            &ItemLocation {
                                key,
                                pointer: *found,
                            },
                        );
                        *found = pointer;
                        *next = new_next;
                    }
                }
            }
        }
    }

    // go through: [..][pubkey_bin][..] and return hashes and lamport sum
    //   slot groups^                ^accounts found in a slot group, sorted by pubkey, higher slot, write_version
    // 1. handle zero lamport accounts
    // 2. pick the highest slot or (slot = and highest version) of each pubkey
    // 3. produce this output:
    //   a. AccountHashesFile: individual account hashes in pubkey order
    //   b. lamport sum
    fn de_dup_accounts_in_parallel(
        &self,
        sorted_data_by_pubkey: &[&[CalculateHashIntermediate]],
        pubkey_bin: usize,
        bins: usize,
        stats: &HashStats,
    ) -> (AccountHashesFile, u64) {
        let binner = PubkeyBinCalculator24::new(bins);

        // working_set hold the lowest items for each slot_group sorted by pubkey descending (min_key is the last)
        let (mut working_set, max_inclusive_num_pubkeys) = Self::initialize_dedup_working_set(
            sorted_data_by_pubkey,
            pubkey_bin,
            bins,
            &binner,
            stats,
        );

        let mut hashes = AccountHashesFile {
            writer: None,
            dir_for_temp_cache_files: self.dir_for_temp_cache_files.clone(),
            capacity: max_inclusive_num_pubkeys * std::mem::size_of::<Hash>(),
        };

        let mut overall_sum = 0;

        while let Some(pointer) = working_set.pop() {
            let key = &sorted_data_by_pubkey[pointer.slot_group_index][pointer.offset].pubkey;

            // get the min item, add lamports, get hash
            let (item, mut next) = Self::get_item(
                sorted_data_by_pubkey,
                pubkey_bin,
                &binner,
                &ItemLocation { key, pointer },
            );

            // add lamports and get hash
            if item.lamports != 0 {
                overall_sum = Self::checked_cast_for_capitalization(
                    item.lamports as u128 + overall_sum as u128,
                );
                // note that we DO have to dedup and avoid zero lamport hashes...
                // todo: probably we could accumulate here instead of writing every hash here and accumulating each hash later (in a map/fold reduce)
                hashes.write(&item.hash.0);
            } else {
                // if lamports == 0, check if they should be included
                if self.zero_lamport_accounts == ZeroLamportAccounts::Included {
                    // For incremental accounts hash, the hash of a zero lamport account is
                    // the hash of its pubkey
                    let hash = blake3::hash(bytemuck::bytes_of(&item.pubkey));
                    let hash = Hash::new_from_array(hash.into());
                    // todo: same as above
                    hashes.write(&hash);
                }
            }

            Self::add_next_item(
                &mut next,
                &mut working_set,
                sorted_data_by_pubkey,
                pubkey_bin,
                &binner,
            );
        }

        (hashes, overall_sum)
    }

    /// input:
    /// vec: group of slot data, ordered by Slot (low to high)
    ///   vec: [..] - items found in that slot range Sorted by: Pubkey, higher Slot, higher Write version (if pubkey =)
    pub fn rest_of_hash_calculation(
        &self,
        sorted_data_by_pubkey: &[&[CalculateHashIntermediate]],
        stats: &mut HashStats,
    ) -> (Hash, u64) {
        let (hashes, total_lamports) = self.de_dup_accounts(
            sorted_data_by_pubkey,
            stats,
            PUBKEY_BINS_FOR_CALCULATING_HASHES,
        );

        let cumulative = CumulativeHashesFromFiles::from_files(hashes);

        let _guard = self.active_stats.activate(ActiveStatItem::HashMerkleTree);
        let mut hash_time = Measure::start("hash");
        // TODO
        let mut _accumulated = Hash::default();
        let mut i = 0;
        while i < cumulative.total_count() {
            let slice = cumulative.get_slice(i);
            slice.iter().for_each(|_hash| {
                // todo: accumulate here if we weren't able to do it earlier
                // accumulated += hash
            });
            i += slice.len();
        }
        let (hash, _) = Self::compute_merkle_root_from_slices(
            cumulative.total_count(),
            MERKLE_FANOUT,
            None,
            |start| cumulative.get_slice(start),
            None,
        );
        hash_time.stop();
        stats.hash_time_total_us += hash_time.as_us();
        (hash, total_lamports)
    }
}

/// How should zero-lamport accounts be treated by the accounts hasher?
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ZeroLamportAccounts {
    Excluded,
    Included,
}

/// Hash of an account
#[repr(transparent)]
#[derive(Default, Debug, Copy, Clone, Eq, PartialEq, Pod, Zeroable, AbiExample)]
pub struct AccountHash(pub Hash);

// Ensure the newtype wrapper never changes size from the underlying Hash
// This also ensures there are no padding bytes, which is required to safely implement Pod
const _: () = assert!(std::mem::size_of::<AccountHash>() == std::mem::size_of::<Hash>());

/// # Account Lattice Hash Q/A
///
/// ## What's account Lattice hash?
/// It is a new way to compute accounts hash based on the Algebra struct Lattice
/// defined on hash field. The field elements are 2048 bytes of hash of
/// individual accounts. This 2048 bytes of hash is computed from blake3 hash.
/// There are two operators defined on the field, add/sub, which are u16 x 1024
/// wrapping add and subtract.
///
/// ## How is the account Lattice hash used?
/// It is used to represent the account state for each slot. Each slot will
/// "add" in the new state of changed accounts and "sub" the old state of the
/// account. This way the accumulated hash of the slot is equal to sum all the
/// accounts state at that slot. The end goal is to replace the current merkle
/// tree hash.
///
/// ## How to handle account deletion and account creation?
/// That's where the identity element in the lattice play it role. All accounts
/// with zero lamport are treated to have the "identity" hash. add/sub such
/// element don't impact the accumulated hash.
///
/// ## Are we going to serialize/deserialize Lattice hash into DB storage?
/// This is an open question. Unlike 32 byte Hash, Lattice hash is 2K. Too
/// expensive to store on disk? Therefore, in current implementation, we don't
/// Serialize Lattice hash into disk. Lattice hash is just computed from the
/// account.
/// In future, when we decide to serialize Lattice hash, we can add
/// [Serialize, Deserialize, BorshSerialize, BorshDeserialize, BorshSchema]
/// to the struct.
///
/// Lattice hash
pub const LT_HASH_BYTES: usize = 2048;
pub const LT_HASH_ELEMENT: usize = 1024;
#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq, Pod, Zeroable, AbiExample)]
pub struct AccountLTHash(pub [u16; LT_HASH_ELEMENT]);

impl Default for AccountLTHash {
    fn default() -> Self {
        Self([0_u16; LT_HASH_ELEMENT])
    }
}

impl AccountLTHash {
    pub fn new(hash_slice: &[u8]) -> Self {
        if hash_slice.len() == LT_HASH_BYTES {
            let ptr = hash_slice.as_ptr() as *const u16;
            let mut output = [0_u16; LT_HASH_ELEMENT];
            unsafe {
                std::ptr::copy_nonoverlapping(ptr, output.as_mut_ptr(), LT_HASH_ELEMENT);
            }
            Self(output)
        } else {
            panic!("wrong size for LTHash");
        }
    }

    pub const fn new_from_array(hash_array: [u16; LT_HASH_ELEMENT]) -> Self {
        Self(hash_array)
    }

    pub fn new_from_reader(mut reader: OutputReader) -> Self {
        let mut output = [0_u16; LT_HASH_ELEMENT];

        let ptr =
            unsafe { slice::from_raw_parts_mut(output.as_mut_ptr() as *mut u8, LT_HASH_BYTES) };
        reader.fill(ptr);
        Self(output)
    }

    pub fn to_u16(self) -> [u16; LT_HASH_ELEMENT] {
        self.0
    }

    pub fn add(&mut self, other: &AccountLTHash) {
        for i in 0..LT_HASH_ELEMENT {
            self.0[i] = self.0[i].wrapping_add(other.0[i]);
        }
    }

    pub fn sub(&mut self, other: &AccountLTHash) {
        for i in 0..LT_HASH_ELEMENT {
            self.0[i] = self.0[i].wrapping_sub(other.0[i]);
        }
    }

    pub fn finalize(&self) -> AccountHash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.as_ref());

        AccountHash(Hash::new_from_array(hasher.finalize().into()))
    }
}

impl AsRef<[u8]> for AccountLTHash {
    fn as_ref(&self) -> &[u8] {
        let ptr = unsafe { slice::from_raw_parts(self.0.as_ptr() as *mut u8, LT_HASH_BYTES) };
        ptr
    }
}

impl AsRef<[u16]> for AccountLTHash {
    fn as_ref(&self) -> &[u16] {
        &self.0[..]
    }
}

impl fmt::Debug for AccountLTHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let u8_slice = AsRef::<[u8]>::as_ref(self);
        write!(f, "{}", bs58::encode(u8_slice).into_string())
    }
}

impl fmt::Display for AccountLTHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let u8_slice = AsRef::<[u8]>::as_ref(self);
        write!(f, "{}", bs58::encode(u8_slice).into_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseLTHashError {
    #[error("string decoded to wrong size for LThash")]
    WrongSize,
    #[error("failed to decoded string to LThash")]
    Invalid,
}

impl FromStr for AccountLTHash {
    type Err = ParseLTHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const MAX_BASE58_LEN: usize = 2798;

        if s.len() > MAX_BASE58_LEN {
            return Err(ParseLTHashError::WrongSize);
        }
        let bytes = bs58::decode(s)
            .into_vec()
            .map_err(|_| ParseLTHashError::Invalid)?;
        if bytes.len() != mem::size_of::<AccountLTHash>() {
            Err(ParseLTHashError::WrongSize)
        } else {
            Ok(AccountLTHash::new(&bytes))
        }
    }
}

/// Hash of accounts
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AccountsHashKind {
    Full(AccountsHash),
    Incremental(IncrementalAccountsHash),
}
impl AccountsHashKind {
    pub fn as_hash(&self) -> &Hash {
        match self {
            AccountsHashKind::Full(AccountsHash(hash))
            | AccountsHashKind::Incremental(IncrementalAccountsHash(hash)) => hash,
        }
    }
}
impl From<AccountsHash> for AccountsHashKind {
    fn from(accounts_hash: AccountsHash) -> Self {
        AccountsHashKind::Full(accounts_hash)
    }
}
impl From<IncrementalAccountsHash> for AccountsHashKind {
    fn from(incremental_accounts_hash: IncrementalAccountsHash) -> Self {
        AccountsHashKind::Incremental(incremental_accounts_hash)
    }
}

/// Hash of accounts
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct AccountsHash(pub Hash);
/// Hash of accounts that includes zero-lamport accounts
/// Used with incremental snapshots
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct IncrementalAccountsHash(pub Hash);

/// Hash of accounts written in a single slot
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct AccountsDeltaHash(pub Hash);

/// Snapshot serde-safe accounts delta hash
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq, AbiExample)]
pub struct SerdeAccountsDeltaHash(pub Hash);

impl From<SerdeAccountsDeltaHash> for AccountsDeltaHash {
    fn from(accounts_delta_hash: SerdeAccountsDeltaHash) -> Self {
        Self(accounts_delta_hash.0)
    }
}
impl From<AccountsDeltaHash> for SerdeAccountsDeltaHash {
    fn from(accounts_delta_hash: AccountsDeltaHash) -> Self {
        Self(accounts_delta_hash.0)
    }
}

/// Snapshot serde-safe accounts hash
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq, AbiExample)]
pub struct SerdeAccountsHash(pub Hash);

impl From<SerdeAccountsHash> for AccountsHash {
    fn from(accounts_hash: SerdeAccountsHash) -> Self {
        Self(accounts_hash.0)
    }
}
impl From<AccountsHash> for SerdeAccountsHash {
    fn from(accounts_hash: AccountsHash) -> Self {
        Self(accounts_hash.0)
    }
}

/// Snapshot serde-safe incremental accounts hash
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq, AbiExample)]
pub struct SerdeIncrementalAccountsHash(pub Hash);

impl From<SerdeIncrementalAccountsHash> for IncrementalAccountsHash {
    fn from(incremental_accounts_hash: SerdeIncrementalAccountsHash) -> Self {
        Self(incremental_accounts_hash.0)
    }
}
impl From<IncrementalAccountsHash> for SerdeIncrementalAccountsHash {
    fn from(incremental_accounts_hash: IncrementalAccountsHash) -> Self {
        Self(incremental_accounts_hash.0)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::accounts_db::AccountsDb, itertools::Itertools,
        solana_sdk::account::AccountSharedData, std::str::FromStr, tempfile::tempdir,
    };

    lazy_static! {
        static ref ACTIVE_STATS: ActiveStats = ActiveStats::default();
    }

    impl<'a> AccountsHasher<'a> {
        fn new(dir_for_temp_cache_files: PathBuf) -> Self {
            Self {
                zero_lamport_accounts: ZeroLamportAccounts::Excluded,
                dir_for_temp_cache_files,
                active_stats: &ACTIVE_STATS,
            }
        }
    }

    impl AccountHashesFile {
        fn new(dir_for_temp_cache_files: PathBuf) -> Self {
            Self {
                writer: None,
                dir_for_temp_cache_files,
                capacity: 1024, /* default 1k for tests */
            }
        }
    }

    impl CumulativeOffsets {
        fn from_raw_2d<T>(raw: &[Vec<Vec<T>>]) -> Self {
            let mut total_count: usize = 0;
            let mut cumulative_offsets = Vec::with_capacity(0);
            for (i, v_outer) in raw.iter().enumerate() {
                for (j, v) in v_outer.iter().enumerate() {
                    let len = v.len();
                    if len > 0 {
                        if cumulative_offsets.is_empty() {
                            // the first inner, non-empty vector we find gives us an approximate rectangular shape
                            cumulative_offsets = Vec::with_capacity(raw.len() * v_outer.len());
                        }
                        cumulative_offsets.push(CumulativeOffset {
                            index: [i, j],
                            start_offset: total_count,
                        });
                        total_count += len;
                    }
                }
            }

            Self {
                cumulative_offsets,
                total_count,
            }
        }
    }

    #[test]
    fn test_find_first_pubkey_in_bin() {
        let stats = HashStats::default();
        for (bins, expected_count) in [1, 2, 4].into_iter().zip([5, 20, 120]) {
            let bins: usize = bins;
            let binner = PubkeyBinCalculator24::new(bins);

            let mut count = 0usize;
            // # pubkeys in each bin are permutations of these
            // 0 means none in this bin
            // large number (20) means the found key will be well before or after the expected index based on an assumption of uniform distribution
            for counts in [0, 1, 2, 20, 0].into_iter().permutations(bins) {
                count += 1;
                let hash_data = counts
                    .iter()
                    .enumerate()
                    .flat_map(|(bin, count)| {
                        (0..*count).map(move |_| {
                            let binner = PubkeyBinCalculator24::new(bins);
                            CalculateHashIntermediate {
                                hash: AccountHash(Hash::default()),
                                lamports: 0,
                                pubkey: binner.lowest_pubkey_from_bin(bin, bins),
                            }
                        })
                    })
                    .collect::<Vec<_>>();
                // look for the first pubkey in each bin
                for (bin, count_in_bin) in counts.iter().enumerate().take(bins) {
                    let first = AccountsHasher::find_first_pubkey_in_bin(
                        &hash_data, bin, bins, &binner, &stats,
                    );
                    // test both functions
                    let first_again = AccountsHasher::binary_search_for_first_pubkey_in_bin(
                        &hash_data, bin, &binner,
                    );
                    assert_eq!(first, first_again);
                    assert_eq!(first.is_none(), count_in_bin == &0);
                    if let Some(first) = first {
                        assert_eq!(binner.bin_from_pubkey(&hash_data[first].pubkey), bin);
                        if first > 0 {
                            assert!(binner.bin_from_pubkey(&hash_data[first - 1].pubkey) < bin);
                        }
                    }
                }
            }
            assert_eq!(
                count, expected_count,
                "too few iterations in test. bins: {bins}"
            );
        }
    }

    #[test]
    fn test_account_hashes_file() {
        let dir_for_temp_cache_files = tempdir().unwrap();
        // 0 hashes
        let mut file = AccountHashesFile::new(dir_for_temp_cache_files.path().to_path_buf());
        assert!(file.get_reader().is_none());
        let hashes = (0..2).map(|i| Hash::new(&[i; 32])).collect::<Vec<_>>();

        // 1 hash
        file.write(&hashes[0]);
        let reader = file.get_reader().unwrap();
        assert_eq!(&[hashes[0]][..], reader.read(0));
        assert!(reader.read(1).is_empty());

        // multiple hashes
        let mut file = AccountHashesFile::new(dir_for_temp_cache_files.path().to_path_buf());
        assert!(file.get_reader().is_none());
        hashes.iter().for_each(|hash| file.write(hash));
        let reader = file.get_reader().unwrap();
        (0..2).for_each(|i| assert_eq!(&hashes[i..], reader.read(i)));
        assert!(reader.read(2).is_empty());
    }

    #[test]
    fn test_cumulative_hashes_from_files() {
        let dir_for_temp_cache_files = tempdir().unwrap();
        (0..4).for_each(|permutation| {
            let hashes = (0..2).map(|i| Hash::new(&[i + 1; 32])).collect::<Vec<_>>();

            let mut combined = Vec::default();

            // 0 hashes
            let file0 = AccountHashesFile::new(dir_for_temp_cache_files.path().to_path_buf());

            // 1 hash
            let mut file1 = AccountHashesFile::new(dir_for_temp_cache_files.path().to_path_buf());
            file1.write(&hashes[0]);
            combined.push(hashes[0]);

            // multiple hashes
            let mut file2 = AccountHashesFile::new(dir_for_temp_cache_files.path().to_path_buf());
            hashes.iter().for_each(|hash| {
                file2.write(hash);
                combined.push(*hash);
            });

            let hashes = if permutation == 0 {
                vec![file0, file1, file2]
            } else if permutation == 1 {
                // include more empty files
                vec![
                    file0,
                    file1,
                    AccountHashesFile::new(dir_for_temp_cache_files.path().to_path_buf()),
                    file2,
                    AccountHashesFile::new(dir_for_temp_cache_files.path().to_path_buf()),
                ]
            } else if permutation == 2 {
                vec![file1, file2]
            } else {
                // swap file2 and 1
                let one = combined.remove(0);
                combined.push(one);
                vec![
                    file2,
                    AccountHashesFile::new(dir_for_temp_cache_files.path().to_path_buf()),
                    AccountHashesFile::new(dir_for_temp_cache_files.path().to_path_buf()),
                    file1,
                ]
            };

            let cumulative = CumulativeHashesFromFiles::from_files(hashes);
            let len = combined.len();
            assert_eq!(cumulative.total_count(), len);
            (0..combined.len()).for_each(|start| {
                let mut retrieved = Vec::default();
                let mut cumulative_start = start;
                // read all data
                while retrieved.len() < (len - start) {
                    let this_one = cumulative.get_slice(cumulative_start);
                    retrieved.extend(this_one.iter());
                    cumulative_start += this_one.len();
                    assert_ne!(0, this_one.len());
                }
                assert_eq!(
                    &combined[start..],
                    &retrieved[..],
                    "permutation: {permutation}"
                );
            });
        });
    }

    #[test]
    fn test_accountsdb_div_ceil() {
        assert_eq!(AccountsHasher::div_ceil(10, 3), 4);
        assert_eq!(AccountsHasher::div_ceil(0, 1), 0);
        assert_eq!(AccountsHasher::div_ceil(0, 5), 0);
        assert_eq!(AccountsHasher::div_ceil(9, 3), 3);
        assert_eq!(AccountsHasher::div_ceil(9, 9), 1);
    }

    #[test]
    #[should_panic(expected = "attempt to divide by zero")]
    fn test_accountsdb_div_ceil_fail() {
        assert_eq!(AccountsHasher::div_ceil(10, 0), 0);
    }

    fn for_rest(original: &[CalculateHashIntermediate]) -> Vec<&[CalculateHashIntermediate]> {
        vec![original]
    }

    #[test]
    fn test_accountsdb_rest_of_hash_calculation() {
        solana_logger::setup();

        let mut account_maps = Vec::new();

        let pubkey = Pubkey::from([11u8; 32]);
        let hash = AccountHash(Hash::new(&[1u8; 32]));
        let val = CalculateHashIntermediate {
            hash,
            lamports: 88,
            pubkey,
        };
        account_maps.push(val);

        // 2nd key - zero lamports, so will be removed
        let pubkey = Pubkey::from([12u8; 32]);
        let hash = AccountHash(Hash::new(&[2u8; 32]));
        let val = CalculateHashIntermediate {
            hash,
            lamports: 0,
            pubkey,
        };
        account_maps.push(val);

        let dir_for_temp_cache_files = tempdir().unwrap();
        let accounts_hash = AccountsHasher::new(dir_for_temp_cache_files.path().to_path_buf());
        let result = accounts_hash
            .rest_of_hash_calculation(&for_rest(&account_maps), &mut HashStats::default());
        let expected_hash = Hash::from_str("8j9ARGFv4W2GfML7d3sVJK2MePwrikqYnu6yqer28cCa").unwrap();
        assert_eq!((result.0, result.1), (expected_hash, 88));

        // 3rd key - with pubkey value before 1st key so it will be sorted first
        let pubkey = Pubkey::from([10u8; 32]);
        let hash = AccountHash(Hash::new(&[2u8; 32]));
        let val = CalculateHashIntermediate {
            hash,
            lamports: 20,
            pubkey,
        };
        account_maps.insert(0, val);

        let result = accounts_hash
            .rest_of_hash_calculation(&for_rest(&account_maps), &mut HashStats::default());
        let expected_hash = Hash::from_str("EHv9C5vX7xQjjMpsJMzudnDTzoTSRwYkqLzY8tVMihGj").unwrap();
        assert_eq!((result.0, result.1), (expected_hash, 108));

        // 3rd key - with later slot
        let pubkey = Pubkey::from([10u8; 32]);
        let hash = AccountHash(Hash::new(&[99u8; 32]));
        let val = CalculateHashIntermediate {
            hash,
            lamports: 30,
            pubkey,
        };
        account_maps.insert(1, val);

        let result = accounts_hash
            .rest_of_hash_calculation(&for_rest(&account_maps), &mut HashStats::default());
        let expected_hash = Hash::from_str("7NNPg5A8Xsg1uv4UFm6KZNwsipyyUnmgCrznP6MBWoBZ").unwrap();
        assert_eq!((result.0, result.1), (expected_hash, 118));
    }

    fn one_range() -> usize {
        1
    }

    fn zero_range() -> usize {
        0
    }

    #[test]
    fn test_accountsdb_de_dup_accounts_zero_chunks() {
        let vec = [vec![CalculateHashIntermediate {
            lamports: 1,
            hash: AccountHash(Hash::default()),
            pubkey: Pubkey::default(),
        }]];
        let temp_vec = vec.to_vec();
        let slice = convert_to_slice(&temp_vec);
        let dir_for_temp_cache_files = tempdir().unwrap();
        let accounts_hasher = AccountsHasher::new(dir_for_temp_cache_files.path().to_path_buf());
        let (mut hashes, lamports) =
            accounts_hasher.de_dup_accounts_in_parallel(&slice, 0, 1, &HashStats::default());
        assert_eq!(&[Hash::default()], hashes.get_reader().unwrap().read(0));
        assert_eq!(lamports, 1);
    }

    fn get_vec_vec(hashes: Vec<AccountHashesFile>) -> Vec<Vec<Hash>> {
        hashes.into_iter().map(get_vec).collect()
    }
    fn get_vec(mut hashes: AccountHashesFile) -> Vec<Hash> {
        hashes
            .get_reader()
            .map(|r| r.read(0).to_vec())
            .unwrap_or_default()
    }

    #[test]
    fn test_accountsdb_de_dup_accounts_empty() {
        solana_logger::setup();
        let dir_for_temp_cache_files = tempdir().unwrap();
        let accounts_hash = AccountsHasher::new(dir_for_temp_cache_files.path().to_path_buf());

        let empty = [];
        let vec = &empty;
        let (hashes, lamports) =
            accounts_hash.de_dup_accounts(vec, &mut HashStats::default(), one_range());
        assert_eq!(
            vec![Hash::default(); 0],
            get_vec_vec(hashes)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
        );
        assert_eq!(lamports, 0);
        let vec = vec![];
        let (hashes, lamports) =
            accounts_hash.de_dup_accounts(&vec, &mut HashStats::default(), zero_range());
        let empty: Vec<Vec<Hash>> = Vec::default();
        assert_eq!(empty, get_vec_vec(hashes));
        assert_eq!(lamports, 0);

        let (hashes, lamports) =
            accounts_hash.de_dup_accounts_in_parallel(&[], 1, 1, &HashStats::default());
        assert_eq!(vec![Hash::default(); 0], get_vec(hashes));
        assert_eq!(lamports, 0);

        let (hashes, lamports) =
            accounts_hash.de_dup_accounts_in_parallel(&[], 2, 1, &HashStats::default());
        assert_eq!(vec![Hash::default(); 0], get_vec(hashes));
        assert_eq!(lamports, 0);
    }

    #[test]
    fn test_accountsdb_de_dup_accounts_from_stores() {
        solana_logger::setup();

        let key_a = Pubkey::from([1u8; 32]);
        let key_b = Pubkey::from([2u8; 32]);
        let key_c = Pubkey::from([3u8; 32]);
        const COUNT: usize = 6;
        let hashes = (0..COUNT).map(|i| AccountHash(Hash::new(&[i as u8; 32])));
        // create this vector
        // abbbcc
        let keys = [key_a, key_b, key_b, key_b, key_c, key_c];

        let accounts: Vec<_> = hashes
            .zip(keys.iter())
            .enumerate()
            .map(|(i, (hash, &pubkey))| CalculateHashIntermediate {
                hash,
                lamports: (i + 1) as u64,
                pubkey,
            })
            .collect();

        type ExpectedType = (String, bool, u64, String);
        let expected:Vec<ExpectedType> = vec![
            // ("key/lamports key2/lamports ...",
            // is_last_slice
            // result lamports
            // result hashes)
            // "a5" = key_a, 5 lamports
            ("a1", false, 1, "[11111111111111111111111111111111]"),
            ("a1b2", false, 3, "[11111111111111111111111111111111, 4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi]"),
            ("a1b2b3", false, 4, "[11111111111111111111111111111111, 8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR]"),
            ("a1b2b3b4", false, 5, "[11111111111111111111111111111111, CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8]"),
            ("a1b2b3b4c5", false, 10, "[11111111111111111111111111111111, CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8, GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ("b2", false, 2, "[4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi]"),
            ("b2b3", false, 3, "[8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR]"),
            ("b2b3b4", false, 4, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8]"),
            ("b2b3b4c5", false, 9, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8, GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ("b3", false, 3, "[8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR]"),
            ("b3b4", false, 4, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8]"),
            ("b3b4c5", false, 9, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8, GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ("b4", false, 4, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8]"),
            ("b4c5", false, 9, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8, GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ("c5", false, 5, "[GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ("a1", true, 1, "[11111111111111111111111111111111]"),
            ("a1b2", true, 3, "[11111111111111111111111111111111, 4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi]"),
            ("a1b2b3", true, 4, "[11111111111111111111111111111111, 8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR]"),
            ("a1b2b3b4", true, 5, "[11111111111111111111111111111111, CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8]"),
            ("a1b2b3b4c5", true, 10, "[11111111111111111111111111111111, CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8, GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ("b2", true, 2, "[4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi]"),
            ("b2b3", true, 3, "[8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR]"),
            ("b2b3b4", true, 4, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8]"),
            ("b2b3b4c5", true, 9, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8, GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ("b3", true, 3, "[8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR]"),
            ("b3b4", true, 4, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8]"),
            ("b3b4c5", true, 9, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8, GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ("b4", true, 4, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8]"),
            ("b4c5", true, 9, "[CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8, GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ("c5", true, 5, "[GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq]"),
            ].into_iter().map(|item| {
                let result: ExpectedType = (
                    item.0.to_string(),
                    item.1,
                    item.2,
                    item.3.to_string(),
                );
                result
            }).collect();

        let dir_for_temp_cache_files = tempdir().unwrap();
        let hash = AccountsHasher::new(dir_for_temp_cache_files.path().to_path_buf());
        let mut expected_index = 0;
        for last_slice in 0..2 {
            for start in 0..COUNT {
                for end in start + 1..COUNT {
                    let is_last_slice = last_slice == 1;
                    let accounts = accounts.clone();
                    let slice = &accounts[start..end];

                    let slice2 = vec![slice.to_vec()];
                    let slice = &slice2[..];
                    let slice_temp = convert_to_slice(&slice2);
                    let (hashes2, lamports2) =
                        hash.de_dup_accounts_in_parallel(&slice_temp, 0, 1, &HashStats::default());
                    let slice3 = convert_to_slice(&slice2);
                    let (hashes3, lamports3) =
                        hash.de_dup_accounts_in_parallel(&slice3, 0, 1, &HashStats::default());
                    let vec = slice.to_vec();
                    let slice4 = convert_to_slice(&vec);
                    let mut max_bin = end - start;
                    if !max_bin.is_power_of_two() {
                        max_bin = 1;
                    }

                    let (hashes4, lamports4) =
                        hash.de_dup_accounts(&slice4, &mut HashStats::default(), max_bin);
                    let vec = slice.to_vec();
                    let slice5 = convert_to_slice(&vec);
                    let (hashes5, lamports5) =
                        hash.de_dup_accounts(&slice5, &mut HashStats::default(), max_bin);
                    let vec = slice.to_vec();
                    let slice5 = convert_to_slice(&vec);
                    let (hashes6, lamports6) =
                        hash.de_dup_accounts(&slice5, &mut HashStats::default(), max_bin);

                    let hashes2 = get_vec(hashes2);
                    let hashes3 = get_vec(hashes3);
                    let hashes4 = get_vec_vec(hashes4);
                    let hashes5 = get_vec_vec(hashes5);
                    let hashes6 = get_vec_vec(hashes6);

                    assert_eq!(hashes2, hashes3);
                    let expected2 = hashes2.clone();
                    assert_eq!(
                        expected2,
                        hashes4.into_iter().flatten().collect::<Vec<_>>(),
                        "last_slice: {last_slice}, start: {start}, end: {end}, slice: {slice:?}"
                    );
                    assert_eq!(
                        expected2.clone(),
                        hashes5.iter().flatten().copied().collect::<Vec<_>>(),
                        "last_slice: {last_slice}, start: {start}, end: {end}, slice: {slice:?}"
                    );
                    assert_eq!(
                        expected2.clone(),
                        hashes6.iter().flatten().copied().collect::<Vec<_>>()
                    );
                    assert_eq!(lamports2, lamports3);
                    assert_eq!(lamports2, lamports4);
                    assert_eq!(lamports2, lamports5);
                    assert_eq!(lamports2, lamports6);

                    let human_readable = slice[0]
                        .iter()
                        .map(|v| {
                            let mut s = (if v.pubkey == key_a {
                                "a"
                            } else if v.pubkey == key_b {
                                "b"
                            } else {
                                "c"
                            })
                            .to_string();

                            s.push_str(&v.lamports.to_string());
                            s
                        })
                        .collect::<String>();

                    let hash_result_as_string = format!("{hashes2:?}");

                    let packaged_result: ExpectedType = (
                        human_readable,
                        is_last_slice,
                        lamports2,
                        hash_result_as_string,
                    );
                    assert_eq!(expected[expected_index], packaged_result);

                    // for generating expected results
                    // error!("{:?},", packaged_result);
                    expected_index += 1;
                }
            }
        }
    }

    #[test]
    fn test_accountsdb_compare_two_hash_entries() {
        solana_logger::setup();
        let pubkey = Pubkey::new_unique();
        let hash = AccountHash(Hash::new_unique());
        let val = CalculateHashIntermediate {
            hash,
            lamports: 1,
            pubkey,
        };

        // slot same, version <
        let hash2 = AccountHash(Hash::new_unique());
        let val2 = CalculateHashIntermediate {
            hash: hash2,
            lamports: 4,
            pubkey,
        };
        assert_eq!(
            std::cmp::Ordering::Equal, // no longer comparing slots or versions
            AccountsHasher::compare_two_hash_entries(&val, &val2)
        );

        // slot same, vers =
        let hash3 = AccountHash(Hash::new_unique());
        let val3 = CalculateHashIntermediate {
            hash: hash3,
            lamports: 2,
            pubkey,
        };
        assert_eq!(
            std::cmp::Ordering::Equal,
            AccountsHasher::compare_two_hash_entries(&val, &val3)
        );

        // slot same, vers >
        let hash4 = AccountHash(Hash::new_unique());
        let val4 = CalculateHashIntermediate {
            hash: hash4,
            lamports: 6,
            pubkey,
        };
        assert_eq!(
            std::cmp::Ordering::Equal, // no longer comparing slots or versions
            AccountsHasher::compare_two_hash_entries(&val, &val4)
        );

        // slot >, version <
        let hash5 = AccountHash(Hash::new_unique());
        let val5 = CalculateHashIntermediate {
            hash: hash5,
            lamports: 8,
            pubkey,
        };
        assert_eq!(
            std::cmp::Ordering::Equal, // no longer comparing slots or versions
            AccountsHasher::compare_two_hash_entries(&val, &val5)
        );
    }

    fn test_de_dup_accounts_in_parallel<'a>(
        account_maps: &'a [&'a [CalculateHashIntermediate]],
    ) -> (AccountHashesFile, u64) {
        let dir_for_temp_cache_files = tempdir().unwrap();
        let accounts_hasher = AccountsHasher::new(dir_for_temp_cache_files.path().to_path_buf());
        accounts_hasher.de_dup_accounts_in_parallel(account_maps, 0, 1, &HashStats::default())
    }

    #[test]
    fn test_accountsdb_remove_zero_balance_accounts() {
        solana_logger::setup();

        let pubkey = Pubkey::new_unique();
        let hash = AccountHash(Hash::new_unique());
        let mut account_maps = Vec::new();
        let val = CalculateHashIntermediate {
            hash,
            lamports: 1,
            pubkey,
        };
        account_maps.push(val);

        let vecs = vec![account_maps.to_vec()];
        let slice = convert_to_slice(&vecs);
        let (hashfile, lamports) = test_de_dup_accounts_in_parallel(&slice);
        assert_eq!(
            (get_vec(hashfile), lamports),
            (vec![val.hash.0], val.lamports)
        );

        // zero original lamports, higher version
        let val = CalculateHashIntermediate {
            hash,
            lamports: 0,
            pubkey,
        };
        account_maps.push(val); // has to be after previous entry since account_maps are in slot order

        let vecs = vec![account_maps.to_vec()];
        let slice = convert_to_slice(&vecs);
        let (hashfile, lamports) = test_de_dup_accounts_in_parallel(&slice);
        assert_eq!((get_vec(hashfile), lamports), (vec![], 0));
    }

    #[test]
    fn test_accountsdb_dup_pubkey_2_chunks() {
        // 2 chunks, a dup pubkey in each chunk
        for reverse in [false, true] {
            let key = Pubkey::new_from_array([1; 32]); // key is BEFORE key2
            let key2 = Pubkey::new_from_array([2; 32]);
            let hash = AccountHash(Hash::new_unique());
            let mut account_maps = Vec::new();
            let mut account_maps2 = Vec::new();
            let val = CalculateHashIntermediate {
                hash,
                lamports: 1,
                pubkey: key,
            };
            account_maps.push(val);
            let val2 = CalculateHashIntermediate {
                hash,
                lamports: 2,
                pubkey: key2,
            };
            account_maps.push(val2);
            let val3 = CalculateHashIntermediate {
                hash,
                lamports: 3,
                pubkey: key2,
            };
            account_maps2.push(val3);

            let mut vecs = vec![account_maps.to_vec(), account_maps2.to_vec()];
            if reverse {
                vecs = vecs.into_iter().rev().collect();
            }
            let slice = convert_to_slice(&vecs);
            let (hashfile, lamports) = test_de_dup_accounts_in_parallel(&slice);
            assert_eq!(
                (get_vec(hashfile), lamports),
                (
                    vec![val.hash.0, if reverse { val2.hash.0 } else { val3.hash.0 }],
                    val.lamports
                        + if reverse {
                            val2.lamports
                        } else {
                            val3.lamports
                        }
                ),
                "reverse: {reverse}"
            );
        }
    }

    #[test]
    fn test_accountsdb_dup_pubkey_2_chunks_backwards() {
        // 2 chunks, a dup pubkey in each chunk
        for reverse in [false, true] {
            let key = Pubkey::new_from_array([3; 32]); // key is AFTER key2
            let key2 = Pubkey::new_from_array([2; 32]);
            let hash = AccountHash(Hash::new_unique());
            let mut account_maps = Vec::new();
            let mut account_maps2 = Vec::new();
            let val2 = CalculateHashIntermediate {
                hash,
                lamports: 2,
                pubkey: key2,
            };
            account_maps.push(val2);
            let val = CalculateHashIntermediate {
                hash,
                lamports: 1,
                pubkey: key,
            };
            account_maps.push(val);
            let val3 = CalculateHashIntermediate {
                hash,
                lamports: 3,
                pubkey: key2,
            };
            account_maps2.push(val3);

            let mut vecs = vec![account_maps.to_vec(), account_maps2.to_vec()];
            if reverse {
                vecs = vecs.into_iter().rev().collect();
            }
            let slice = convert_to_slice(&vecs);
            let (hashfile, lamports) = test_de_dup_accounts_in_parallel(&slice);
            assert_eq!(
                (get_vec(hashfile), lamports),
                (
                    vec![if reverse { val2.hash.0 } else { val3.hash.0 }, val.hash.0],
                    val.lamports
                        + if reverse {
                            val2.lamports
                        } else {
                            val3.lamports
                        }
                ),
                "reverse: {reverse}"
            );
        }
    }

    #[test]
    fn test_accountsdb_cumulative_offsets1_d() {
        let input = vec![vec![0, 1], vec![], vec![2, 3, 4], vec![]];
        let cumulative = CumulativeOffsets::from_raw(&input);

        let src: Vec<_> = input.clone().into_iter().flatten().collect();
        let len = src.len();
        assert_eq!(cumulative.total_count, len);
        assert_eq!(cumulative.cumulative_offsets.len(), 2); // 2 non-empty vectors

        const DIMENSION: usize = 0;
        assert_eq!(cumulative.cumulative_offsets[0].index[DIMENSION], 0);
        assert_eq!(cumulative.cumulative_offsets[1].index[DIMENSION], 2);

        assert_eq!(cumulative.cumulative_offsets[0].start_offset, 0);
        assert_eq!(cumulative.cumulative_offsets[1].start_offset, 2);

        for start in 0..len {
            let slice = cumulative.get_slice(&input, start);
            let len = slice.len();
            assert!(len > 0);
            assert_eq!(&src[start..(start + len)], slice);
        }

        let input = vec![vec![], vec![0, 1], vec![], vec![2, 3, 4], vec![]];
        let cumulative = CumulativeOffsets::from_raw(&input);

        let src: Vec<_> = input.clone().into_iter().flatten().collect();
        let len = src.len();
        assert_eq!(cumulative.total_count, len);
        assert_eq!(cumulative.cumulative_offsets.len(), 2); // 2 non-empty vectors

        assert_eq!(cumulative.cumulative_offsets[0].index[DIMENSION], 1);
        assert_eq!(cumulative.cumulative_offsets[1].index[DIMENSION], 3);

        assert_eq!(cumulative.cumulative_offsets[0].start_offset, 0);
        assert_eq!(cumulative.cumulative_offsets[1].start_offset, 2);

        for start in 0..len {
            let slice = cumulative.get_slice(&input, start);
            let len = slice.len();
            assert!(len > 0);
            assert_eq!(&src[start..(start + len)], slice);
        }

        let input: Vec<Vec<u32>> = vec![vec![]];
        let cumulative = CumulativeOffsets::from_raw(&input);

        let len = input.into_iter().flatten().count();
        assert_eq!(cumulative.total_count, len);
        assert_eq!(cumulative.cumulative_offsets.len(), 0); // 2 non-empty vectors
    }

    #[should_panic(expected = "is_empty")]
    #[test]
    fn test_accountsdb_cumulative_find_empty() {
        let input = CumulativeOffsets {
            cumulative_offsets: vec![],
            total_count: 0,
        };
        input.find(0);
    }

    #[test]
    fn test_accountsdb_cumulative_find() {
        let input = CumulativeOffsets {
            cumulative_offsets: vec![CumulativeOffset {
                index: [0; 2],
                start_offset: 0,
            }],
            total_count: 0,
        };
        assert_eq!(input.find(0), (0, &input.cumulative_offsets[0]));

        let input = CumulativeOffsets {
            cumulative_offsets: vec![
                CumulativeOffset {
                    index: [0; 2],
                    start_offset: 0,
                },
                CumulativeOffset {
                    index: [1; 2],
                    start_offset: 2,
                },
            ],
            total_count: 0,
        };
        assert_eq!(input.find(0), (0, &input.cumulative_offsets[0])); // = first start_offset
        assert_eq!(input.find(1), (1, &input.cumulative_offsets[0])); // > first start_offset
        assert_eq!(input.find(2), (0, &input.cumulative_offsets[1])); // = last start_offset
        assert_eq!(input.find(3), (1, &input.cumulative_offsets[1])); // > last start_offset
    }

    #[test]
    fn test_accountsdb_cumulative_offsets2_d() {
        let input: Vec<Vec<Vec<u64>>> = vec![vec![vec![0, 1], vec![], vec![2, 3, 4], vec![]]];
        let cumulative = CumulativeOffsets::from_raw_2d(&input);

        let src: Vec<_> = input.clone().into_iter().flatten().flatten().collect();
        let len = src.len();
        assert_eq!(cumulative.total_count, len);
        assert_eq!(cumulative.cumulative_offsets.len(), 2); // 2 non-empty vectors

        const DIMENSION_0: usize = 0;
        const DIMENSION_1: usize = 1;
        assert_eq!(cumulative.cumulative_offsets[0].index[DIMENSION_0], 0);
        assert_eq!(cumulative.cumulative_offsets[0].index[DIMENSION_1], 0);
        assert_eq!(cumulative.cumulative_offsets[1].index[DIMENSION_0], 0);
        assert_eq!(cumulative.cumulative_offsets[1].index[DIMENSION_1], 2);

        assert_eq!(cumulative.cumulative_offsets[0].start_offset, 0);
        assert_eq!(cumulative.cumulative_offsets[1].start_offset, 2);

        for start in 0..len {
            let slice: &[u64] = cumulative.get_slice(&input, start);
            let len = slice.len();
            assert!(len > 0);
            assert_eq!(&src[start..(start + len)], slice);
        }

        let input = vec![vec![vec![], vec![0, 1], vec![], vec![2, 3, 4], vec![]]];
        let cumulative = CumulativeOffsets::from_raw_2d(&input);

        let src: Vec<_> = input.clone().into_iter().flatten().flatten().collect();
        let len = src.len();
        assert_eq!(cumulative.total_count, len);
        assert_eq!(cumulative.cumulative_offsets.len(), 2); // 2 non-empty vectors

        assert_eq!(cumulative.cumulative_offsets[0].index[DIMENSION_0], 0);
        assert_eq!(cumulative.cumulative_offsets[0].index[DIMENSION_1], 1);
        assert_eq!(cumulative.cumulative_offsets[1].index[DIMENSION_0], 0);
        assert_eq!(cumulative.cumulative_offsets[1].index[DIMENSION_1], 3);

        assert_eq!(cumulative.cumulative_offsets[0].start_offset, 0);
        assert_eq!(cumulative.cumulative_offsets[1].start_offset, 2);

        for start in 0..len {
            let slice: &[u64] = cumulative.get_slice(&input, start);
            let len = slice.len();
            assert!(len > 0);
            assert_eq!(&src[start..(start + len)], slice);
        }

        let input: Vec<Vec<Vec<u32>>> = vec![vec![]];
        let cumulative = CumulativeOffsets::from_raw_2d(&input);

        let len = input.into_iter().flatten().count();
        assert_eq!(cumulative.total_count, len);
        assert_eq!(cumulative.cumulative_offsets.len(), 0); // 2 non-empty vectors

        let input = vec![
            vec![vec![0, 1]],
            vec![vec![]],
            vec![vec![], vec![2, 3, 4], vec![]],
        ];
        let cumulative = CumulativeOffsets::from_raw_2d(&input);

        let src: Vec<_> = input.clone().into_iter().flatten().flatten().collect();
        let len = src.len();
        assert_eq!(cumulative.total_count, len);
        assert_eq!(cumulative.cumulative_offsets.len(), 2); // 2 non-empty vectors

        assert_eq!(cumulative.cumulative_offsets[0].index[DIMENSION_0], 0);
        assert_eq!(cumulative.cumulative_offsets[0].index[DIMENSION_1], 0);
        assert_eq!(cumulative.cumulative_offsets[1].index[DIMENSION_0], 2);
        assert_eq!(cumulative.cumulative_offsets[1].index[DIMENSION_1], 1);

        assert_eq!(cumulative.cumulative_offsets[0].start_offset, 0);
        assert_eq!(cumulative.cumulative_offsets[1].start_offset, 2);

        for start in 0..len {
            let slice: &[u64] = cumulative.get_slice(&input, start);
            let len = slice.len();
            assert!(len > 0);
            assert_eq!(&src[start..(start + len)], slice);
        }
    }

    fn test_hashing_larger(hashes: Vec<(Pubkey, Hash)>, fanout: usize) -> Hash {
        let result = AccountsHasher::compute_merkle_root(hashes.clone(), fanout);
        let reduced: Vec<_> = hashes.iter().map(|x| x.1).collect();
        let result2 = test_hashing(reduced, fanout);
        assert_eq!(result, result2, "len: {}", hashes.len());
        result
    }

    fn test_hashing(hashes: Vec<Hash>, fanout: usize) -> Hash {
        let temp: Vec<_> = hashes.iter().map(|h| (Pubkey::default(), *h)).collect();
        let result = AccountsHasher::compute_merkle_root(temp, fanout);
        let reduced: Vec<_> = hashes.clone();
        let result2 = AccountsHasher::compute_merkle_root_from_slices(
            hashes.len(),
            fanout,
            None,
            |start| &reduced[start..],
            None,
        );
        assert_eq!(result, result2.0, "len: {}", hashes.len());

        let result2 = AccountsHasher::compute_merkle_root_from_slices(
            hashes.len(),
            fanout,
            Some(1),
            |start| &reduced[start..],
            None,
        );
        assert_eq!(result, result2.0, "len: {}", hashes.len());

        let max = std::cmp::min(reduced.len(), fanout * 2);
        for left in 0..max {
            for right in left + 1..max {
                let src = vec![
                    vec![reduced[0..left].to_vec(), reduced[left..right].to_vec()],
                    vec![reduced[right..].to_vec()],
                ];
                let offsets = CumulativeOffsets::from_raw_2d(&src);

                let get_slice = |start: usize| -> &[Hash] { offsets.get_slice(&src, start) };
                let result2 = AccountsHasher::compute_merkle_root_from_slices(
                    offsets.total_count,
                    fanout,
                    None,
                    get_slice,
                    None,
                );
                assert_eq!(result, result2.0);
            }
        }
        result
    }

    #[test]
    fn test_accountsdb_compute_merkle_root_large() {
        solana_logger::setup();

        // handle fanout^x -1, +0, +1 for a few 'x's
        const FANOUT: usize = 3;
        let mut hash_counts: Vec<_> = (1..6)
            .flat_map(|x| {
                let mark = FANOUT.pow(x);
                vec![mark - 1, mark, mark + 1]
            })
            .collect();

        // saturate the test space for threshold to threshold + target
        // this hits right before we use the 3 deep optimization and all the way through all possible partial last chunks
        let target = FANOUT.pow(3);
        let threshold = target * FANOUT;
        hash_counts.extend(threshold - 1..=threshold + target);

        for hash_count in hash_counts {
            let hashes: Vec<_> = (0..hash_count).map(|_| Hash::new_unique()).collect();

            test_hashing(hashes, FANOUT);
        }
    }

    #[test]
    fn test_accountsdb_compute_merkle_root() {
        solana_logger::setup();

        let expected_results = vec![
            (0, 0, "GKot5hBsd81kMupNCXHaqbhv3huEbxAFMLnpcX2hniwn", 0),
            (0, 1, "8unXKJYTxrR423HgQxbDmx29mFri1QNrzVKKDxEfc6bj", 0),
            (0, 2, "6QfkevXLLqbfAaR1kVjvMLFtEXvNUVrpmkwXqgsYtCFW", 1),
            (0, 3, "G3FrJd9JrXcMiqChTSfvEdBL2sCPny3ebiUy9Xxbn7a2", 3),
            (0, 4, "G3sZXHhwoCFuNyWy7Efffr47RBW33ibEp7b2hqNDmXdu", 6),
            (0, 5, "78atJJYpokAPKMJwHxUW8SBDvPkkSpTBV7GiB27HwosJ", 10),
            (0, 6, "7c9SM2BmCRVVXdrEdKcMK91MviPqXqQMd8QAb77tgLEy", 15),
            (0, 7, "3hsmnZPhf22UvBLiZ4dVa21Qsdh65CCrtYXsb8MxoVAa", 21),
            (0, 8, "5bwXUiC6RCRhb8fqvjvUXT6waU25str3UXA3a6Aq1jux", 28),
            (0, 9, "3NNtQKH6PaYpCnFBtyi2icK9eYX3YM5pqA3SKaXtUNzu", 36),
            (1, 0, "GKot5hBsd81kMupNCXHaqbhv3huEbxAFMLnpcX2hniwn", 0),
            (1, 1, "4GWVCsnEu1iRyxjAB3F7J7C4MMvcoxFWtP9ihvwvDgxY", 0),
            (1, 2, "8ML8Te6Uw2mipFr2v9sMZDcziXzhVqJo2qeMJohg1CJx", 1),
            (1, 3, "AMEuC3AgqAeRBGBhSfTmuMdfbAiXJnGmKv99kHmcAE1H", 3),
            (1, 4, "HEnDuJLHpsQfrApimGrovTqPEF6Vkrx2dKFr3BDtYzWx", 6),
            (1, 5, "6rH69iP2yM1o565noZN1EqjySW4PhYUskz3c5tXePUfV", 10),
            (1, 6, "7qEQMEXdfSPjbZ3q4cuuZwebDMvTvuaQ3dBiHoDUKo9a", 15),
            (1, 7, "GDJz7LSKYjqqz6ujCaaQRJRmQ7TLNCwYJhdT84qT4qwk", 21),
            (1, 8, "HT9krPLVTo3rr5WZQBQFrbqWs8SbYScXfnt8EVuobboM", 28),
            (1, 9, "8y2pMgqMdRsvqw6BQXm6wtz3qxGPss72i6H6gVpPyeda", 36),
        ];

        let mut expected_index = 0;
        let start = 0;
        let default_fanout = 2;
        // test 0..3 recursions (at fanout = 2) and 1 item remainder. The internals have 1 special case first loop and subsequent loops are the same types.
        let iterations = default_fanout * default_fanout * default_fanout + 2;
        for pass in 0..2 {
            let fanout = if pass == 0 {
                default_fanout
            } else {
                MERKLE_FANOUT
            };
            for count in start..iterations {
                let mut input: Vec<_> = (0..count)
                    .map(|i| {
                        let key = Pubkey::from([(pass * iterations + count) as u8; 32]);
                        let hash = Hash::new(&[(pass * iterations + count + i + 1) as u8; 32]);
                        (key, hash)
                    })
                    .collect();

                let result = if pass == 0 {
                    test_hashing_larger(input, fanout)
                } else {
                    // this sorts inside
                    let early_result = AccountsHasher::accumulate_account_hashes(
                        input
                            .iter()
                            .map(|i| (i.0, AccountHash(i.1)))
                            .collect::<Vec<_>>(),
                    );

                    input.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));
                    let result = AccountsHasher::compute_merkle_root(input, fanout);
                    assert_eq!(early_result, result);
                    result
                };
                // compare against captured, expected results for hash (and lamports)
                assert_eq!(
                    (
                        pass,
                        count,
                        &*(result.to_string()),
                        expected_results[expected_index].3
                    ), // we no longer calculate lamports
                    expected_results[expected_index]
                );
                expected_index += 1;
            }
        }
    }

    #[test]
    #[should_panic(expected = "overflow is detected while summing capitalization")]
    fn test_accountsdb_lamport_overflow() {
        solana_logger::setup();

        let offset = 2;
        let input = vec![
            CalculateHashIntermediate {
                hash: AccountHash(Hash::new(&[1u8; 32])),
                lamports: u64::MAX - offset,
                pubkey: Pubkey::new_unique(),
            },
            CalculateHashIntermediate {
                hash: AccountHash(Hash::new(&[2u8; 32])),
                lamports: offset + 1,
                pubkey: Pubkey::new_unique(),
            },
        ];
        let dir_for_temp_cache_files = tempdir().unwrap();
        let accounts_hasher = AccountsHasher::new(dir_for_temp_cache_files.path().to_path_buf());
        accounts_hasher.de_dup_accounts_in_parallel(
            &convert_to_slice(&[input]),
            0,
            1,
            &HashStats::default(),
        );
    }

    fn convert_to_slice(
        input: &[Vec<CalculateHashIntermediate>],
    ) -> Vec<&[CalculateHashIntermediate]> {
        input.iter().map(|v| &v[..]).collect::<Vec<_>>()
    }

    #[test]
    #[should_panic(expected = "overflow is detected while summing capitalization")]
    fn test_accountsdb_lamport_overflow2() {
        solana_logger::setup();

        let offset = 2;
        let input = vec![
            vec![CalculateHashIntermediate {
                hash: AccountHash(Hash::new(&[1u8; 32])),
                lamports: u64::MAX - offset,
                pubkey: Pubkey::new_unique(),
            }],
            vec![CalculateHashIntermediate {
                hash: AccountHash(Hash::new(&[2u8; 32])),
                lamports: offset + 1,
                pubkey: Pubkey::new_unique(),
            }],
        ];
        let dir_for_temp_cache_files = tempdir().unwrap();
        let accounts_hasher = AccountsHasher::new(dir_for_temp_cache_files.path().to_path_buf());
        accounts_hasher.de_dup_accounts(
            &convert_to_slice(&input),
            &mut HashStats::default(),
            2, // accounts above are in 2 groups
        );
    }

    #[test]
    fn test_lt_account_hash() {
        let h = AccountLTHash::default();
        assert!(h.0.iter().all(|&x| x == 0));
        assert!(h.0.len() == LT_HASH_ELEMENT);

        let owner = Pubkey::new_unique();
        let (key, account) = (Pubkey::new_unique(), AccountSharedData::new(0, 0, &owner));
        let h = AccountsDb::lt_hash_account(&account, &key);
        assert!(h.0.iter().all(|&x| x == 0));
        assert!(h.0.len() == LT_HASH_ELEMENT);
    }

    /// Test lattice hash computation (ported from FD implementation)
    /// https://github.com/firedancer-io/firedancer/blob/lthash/src/ballet/lthash/test_fd_lthash.c
    #[test]
    fn test_lt_hash() {
        let get_lt_hash = |input: &[u8]| -> AccountLTHash {
            let mut hasher = blake3::Hasher::new();
            hasher.update(input);
            AccountLTHash::new_from_reader(hasher.finalize_xof())
        };

        // lt hash for "hello"
        const LTHASH_HELLO: [u16; 1024] = [
            0x8fea, 0x3d16, 0x86b3, 0x9282, 0x445e, 0xc591, 0x8de5, 0xb34b, 0x6e50, 0xc1f8, 0xb74e,
            0x868a, 0x08e9, 0x62c5, 0x674a, 0x0f20, 0x92e9, 0x5f40, 0x780d, 0x595b, 0x2e9a, 0x8733,
            0xd3f6, 0x014d, 0xccfa, 0xb2fe, 0xb62f, 0xef97, 0xd53f, 0x4135, 0x1a24, 0x8c33, 0x88c6,
            0x5676, 0xb58a, 0xe5c6, 0xab24, 0xfebc, 0x1e88, 0x4e5b, 0xc91a, 0x6f33, 0x933f, 0x412d,
            0x4822, 0x82c9, 0x3695, 0x9f69, 0xa107, 0xceb1, 0xff35, 0xe0df, 0x5dbe, 0xc000, 0xa883,
            0xd2df, 0x9a9c, 0x0343, 0x37d1, 0xd74c, 0x6a0e, 0xecbc, 0x6b6e, 0x6c79, 0xac92, 0x0905,
            0xc1cf, 0xaa9d, 0x6969, 0x736e, 0xcf4c, 0x0029, 0xcf70, 0x8f05, 0xde0f, 0x3fc9, 0x1db6,
            0x6d09, 0x2e08, 0xf4aa, 0x7208, 0x2cc1, 0x8cfb, 0x276e, 0xd62e, 0x2211, 0xf254, 0x8518,
            0x4d07, 0x1594, 0xf13f, 0xab12, 0xcc65, 0x4d4a, 0xceba, 0xfe93, 0x589f, 0x9f4e, 0xe7ea,
            0x63a8, 0xe612, 0x4ced, 0x58a5, 0x43b3, 0x39f6, 0x457c, 0x474f, 0x9aff, 0x5124, 0x63f6,
            0x450d, 0x3fc2, 0x9ccf, 0xf0c6, 0xc69f, 0x2bd3, 0x7a5d, 0x9574, 0x2f2c, 0xf934, 0xcc03,
            0x9342, 0x9998, 0x0da9, 0x6dd1, 0x460d, 0x3e00, 0xcdde, 0xf14d, 0x06ec, 0x6b74, 0x9551,
            0x68c4, 0x0f94, 0x4ac6, 0xed49, 0xd886, 0x24cb, 0x2a29, 0xf4a4, 0x3a83, 0x1f81, 0xe97a,
            0xfa1e, 0xb1c5, 0xfcd5, 0xb24c, 0xdb92, 0x2b62, 0xa4f1, 0x498e, 0xf00d, 0x63be, 0x7f6e,
            0x2c33, 0xdc3e, 0xb0fb, 0xe854, 0x8ee3, 0x5d95, 0xc613, 0x670b, 0xf4aa, 0x5570, 0x04bc,
            0xf606, 0x664f, 0xe5ec, 0xd65b, 0x0ea1, 0xf37c, 0x7745, 0x809b, 0x031e, 0xed80, 0x7254,
            0x211b, 0x0cce, 0x94e1, 0x6bf6, 0x95b1, 0x49ba, 0x64c0, 0x8ec9, 0x3b27, 0x5f21, 0xafc8,
            0x3b86, 0x2ea5, 0x8c30, 0x168e, 0xc147, 0x1fd5, 0x1637, 0x88f5, 0x9321, 0x63aa, 0xaae5,
            0x33bb, 0xd983, 0xb09a, 0xf24e, 0xa1e5, 0x2b39, 0xd434, 0x7135, 0x61ed, 0x57ad, 0x5940,
            0xe53f, 0x727d, 0x4882, 0x8c44, 0xa61b, 0x1b9f, 0xcee4, 0xf462, 0xc875, 0xc019, 0x9310,
            0x7dc2, 0xf55c, 0xcb36, 0x9505, 0xebb5, 0x8a2b, 0x2b07, 0x0a36, 0x3890, 0x54c8, 0x5a76,
            0xece7, 0x96f1, 0xe3f7, 0x6d99, 0x83e4, 0xff35, 0x1d04, 0x8783, 0xbf2e, 0xb846, 0x79a9,
            0x69ba, 0xb980, 0x28f6, 0x2325, 0x7d13, 0xc44c, 0xacba, 0x134e, 0xa877, 0x6b67, 0x8027,
            0xba94, 0xf564, 0x2174, 0xf985, 0x91c8, 0xd568, 0x319f, 0x6d4e, 0xa59b, 0xd344, 0x4a67,
            0x801d, 0x7aeb, 0x20c0, 0xba23, 0x9744, 0xdd93, 0x4cc5, 0x1148, 0xdf86, 0xad19, 0x06b7,
            0xa824, 0x8e56, 0x2cab, 0x9ad1, 0x5ec0, 0xd57c, 0x0f2b, 0x8d85, 0x65e2, 0xd9c0, 0xc824,
            0x3cae, 0xed26, 0x5c7c, 0x41f9, 0x4767, 0xf730, 0xe210, 0x2926, 0xb68f, 0xcf36, 0x22b9,
            0x5f1b, 0x4ae4, 0xcdcd, 0xe69a, 0x9f4c, 0x1036, 0x8e7c, 0x48de, 0xee0f, 0xbcbd, 0x6bc7,
            0x067a, 0x35e6, 0x98fa, 0x2dcb, 0xa442, 0xbcd0, 0xa02c, 0xc746, 0x60b9, 0x479e, 0x6f56,
            0xff1a, 0xe6f0, 0xef75, 0x5dad, 0x2096, 0xbd07, 0x96e2, 0x2bc6, 0xee33, 0xd122, 0x05f7,
            0x2177, 0x2dbc, 0x729b, 0xfdf0, 0x2c18, 0x800c, 0xdb7d, 0xfb19, 0x0002, 0x3895, 0x5b72,
            0xfbe7, 0x16ce, 0x671f, 0x2175, 0x7c84, 0xc8dc, 0x9690, 0xf594, 0x31b4, 0x47f3, 0xe3f2,
            0x8911, 0x747d, 0x25c2, 0x480a, 0x16ff, 0xba50, 0x8bcb, 0xe9d7, 0xec54, 0x7df4, 0x4b9a,
            0xf4bb, 0x3100, 0x86cc, 0x62c2, 0x9b73, 0x06d7, 0x157b, 0x0922, 0xab9e, 0x83a6, 0x2f28,
            0x30ce, 0x3eff, 0x5134, 0xc9d5, 0x74ae, 0x295c, 0x9af8, 0x482a, 0x61dc, 0xe555, 0x9c7c,
            0x57de, 0xfe56, 0xd898, 0x19c6, 0x444f, 0x9636, 0x9297, 0xea84, 0xeaba, 0xce24, 0x6dc0,
            0x14c3, 0x6e7d, 0x2a65, 0x3bb5, 0x679d, 0x22a1, 0x8ea1, 0xc564, 0xca61, 0x0b2a, 0x38ea,
            0xe029, 0xcf07, 0x4280, 0xff2a, 0x8697, 0x8d30, 0x185b, 0x919a, 0x8f7c, 0x046c, 0x9390,
            0x50ab, 0xcb51, 0x2334, 0x616f, 0x998f, 0x1d2d, 0xd294, 0x74f1, 0x822c, 0xe50d, 0xdcc6,
            0xbafc, 0x7d92, 0xe202, 0xe28e, 0x2e19, 0xecaa, 0x7cf5, 0x25aa, 0x7a1a, 0x389a, 0xc189,
            0x6af0, 0x6fa3, 0x16c3, 0xa318, 0x8cb5, 0x348e, 0x627b, 0xd144, 0x7d8d, 0xc43c, 0xca5b,
            0xf4bd, 0xb174, 0x4734, 0x3520, 0xbeb9, 0x4f79, 0xa628, 0xe4bd, 0x1bc7, 0xa9f4, 0x3ad2,
            0x959b, 0xe178, 0x1ba2, 0x48bb, 0x5e79, 0xd594, 0xf41e, 0x78ce, 0x685c, 0x79d4, 0xedae,
            0xe11d, 0x2172, 0xb9ab, 0x5ca2, 0xf9ff, 0x2812, 0x66b7, 0xed6d, 0x7eff, 0x960f, 0x4844,
            0x9484, 0x504a, 0x5b29, 0xca8b, 0xdafd, 0xa6b7, 0xef3a, 0xe2e0, 0xa137, 0x1b05, 0x16c2,
            0xefbd, 0x06ac, 0xf3f1, 0xa94f, 0xcade, 0x7087, 0x2ec9, 0x6543, 0x49a1, 0xf4c3, 0x3157,
            0xed65, 0xfc85, 0xefd4, 0x30b8, 0xa5e8, 0x093f, 0xcbe2, 0x8e2b, 0x2fd4, 0xae39, 0x3e37,
            0x37c5, 0xf02f, 0xf643, 0xc03e, 0xe4d0, 0xe305, 0xfd1a, 0x698d, 0x1285, 0x19de, 0x1582,
            0x251f, 0xe136, 0x3eec, 0x862b, 0xbf4d, 0xab67, 0x0c90, 0x3eb5, 0x58d0, 0xc300, 0x7f93,
            0x03e1, 0xf2f9, 0x78fd, 0x93b6, 0x5add, 0x865a, 0x8b20, 0x89e4, 0x7585, 0x6e40, 0x5a8a,
            0x8623, 0x7335, 0xa9e1, 0xfecf, 0x83cb, 0xe9de, 0xf07c, 0x36ca, 0x5a7b, 0x9fff, 0xe419,
            0x8e48, 0xa704, 0xbcab, 0x44ae, 0x6dfa, 0x810c, 0x94f4, 0x62fb, 0xa34e, 0xa9a5, 0x1d13,
            0x98a9, 0x88ba, 0x7bc2, 0x7a59, 0x188a, 0x1855, 0xd27d, 0x6781, 0xcf08, 0xde49, 0x5588,
            0x5c8b, 0x1f4a, 0xd22b, 0x3959, 0xe754, 0xf071, 0xdfc2, 0xf352, 0x255c, 0x2d36, 0x59d0,
            0x4621, 0x1ed0, 0xa0b5, 0x457d, 0xd3d7, 0xd137, 0x10ca, 0xeeb1, 0xec30, 0x96af, 0x9be5,
            0x2181, 0xe570, 0x8a33, 0x137e, 0x861e, 0xd155, 0x950d, 0xc6e4, 0x5c1f, 0xe4dc, 0x4466,
            0x7078, 0x75a5, 0x7a51, 0x1339, 0xa1a8, 0xcb89, 0xf383, 0xabf0, 0x0170, 0xbb1d, 0xea76,
            0xe491, 0xf911, 0xdc42, 0xec04, 0x82b8, 0xeadd, 0xc890, 0x505c, 0xafa7, 0x42cb, 0xfd99,
            0x127e, 0x0724, 0xd4f9, 0x94ef, 0xf060, 0x67fe, 0x038d, 0x2876, 0xb812, 0xbf05, 0xe904,
            0x003e, 0x2ee4, 0xe8f5, 0x0a66, 0xd790, 0x3ccc, 0x28be, 0xdbc2, 0x073c, 0xd4a5, 0x904c,
            0x60ad, 0x4f67, 0x77ac, 0xae49, 0x2d6c, 0x9220, 0xde9c, 0x2a2b, 0xf99c, 0xb54f, 0x8290,
            0x2e7d, 0x0ca1, 0xf79b, 0xc6ff, 0x3e6e, 0x8eb4, 0x66b1, 0xc6e6, 0x600f, 0xda08, 0xa933,
            0x2cad, 0x308a, 0x93f2, 0x4f70, 0x72d3, 0x56e0, 0x4ddd, 0x682c, 0x589f, 0xd461, 0x06ad,
            0x4e9a, 0x1af7, 0x901c, 0xa1d4, 0xb990, 0xbbcc, 0xdcbb, 0xe46f, 0xe585, 0x9800, 0x86e6,
            0xa735, 0xac0f, 0xb666, 0xaeac, 0x6e00, 0x8b36, 0xc4ce, 0x7261, 0xf078, 0xb42a, 0x86fb,
            0xd4d8, 0x1402, 0xd7ac, 0x69c6, 0x8b29, 0x66ce, 0x512d, 0x93f8, 0x811b, 0x7b2c, 0x1a3b,
            0x88fb, 0x8ca2, 0x197e, 0xbd7b, 0x5c5c, 0xf2c3, 0x803b, 0xe9f2, 0x6fd2, 0x8c05, 0x6966,
            0x2249, 0xceab, 0xe42b, 0x8195, 0x9ddc, 0x79ee, 0x1e35, 0x3fd4, 0x6fc4, 0x9b26, 0x85b0,
            0x45a4, 0x5a6b, 0xf43b, 0x0f07, 0x3104, 0x463d, 0x710a, 0x288e, 0x0dcd, 0x8f1a, 0xa307,
            0x6790, 0x1f2e, 0x991a, 0x7fcc, 0x241a, 0x80d9, 0x9f22, 0xac19, 0x0015, 0x5690, 0x45ba,
            0x4a3f, 0x84f1, 0x01c5, 0xc2b8, 0xa512, 0xffc0, 0xebbd, 0x3c5f, 0x66dc, 0x9fdd, 0xe066,
            0x5b39, 0x2fa1, 0x9432, 0xad65, 0xf397, 0x528a, 0x0c94, 0xe646, 0xbeb5, 0xe91c, 0x7d24,
            0x305c, 0x2c7b, 0x3f93, 0x860e, 0x6e39, 0x953a, 0xb010, 0xbb1b, 0x15a2, 0x369b, 0xf840,
            0xa258, 0xb39a, 0x522b, 0xedbb, 0x7fb9, 0xb94c, 0x45d0, 0x34c0, 0xd516, 0xb52d, 0xdce1,
            0x35e4, 0x3801, 0x3e5c, 0x6826, 0x3b4e, 0xc688, 0xe612, 0x64a8, 0x7898, 0xd07f, 0xa93e,
            0x0f42, 0x9392, 0xa877, 0xd68f, 0xd947, 0x7615, 0xac5e, 0x6f1c, 0x3a42, 0x04c8, 0x993e,
            0x53e5, 0x272e, 0x3021, 0xa3d2, 0xfc24, 0xbd1e, 0xf109, 0x3b8f, 0x6566, 0x48f9, 0x4ef5,
            0x777d, 0xcbaa, 0x029e, 0x8867, 0xda07, 0xa941, 0xeb45, 0x8ad2, 0x9c78, 0xa7c9, 0xdf67,
            0x2ec0, 0x8c0b, 0x6827, 0x18ca, 0x78c2, 0xc9df, 0x8a0e, 0x2aae, 0x4e31, 0xa7ec, 0xd0e5,
            0x748c, 0x1556, 0x44ad, 0xec45, 0x9e48, 0x13d1, 0x74ae, 0x1382, 0x6fdd, 0x6d15, 0x39b9,
            0x4a8a, 0xe31d, 0x4732, 0xb215, 0x5b5e, 0x5b7a, 0x5981, 0x4e94, 0x2ccd, 0x12b6, 0x5072,
            0x4e2b, 0x078f, 0x6896, 0xec47, 0x1165, 0x2625, 0x7fd3, 0xe652, 0xb05f, 0x6fc8, 0xfcb0,
            0xf199, 0xef36, 0x89db, 0xb274, 0x3e7c, 0x9985, 0xbc7a, 0xbd5e, 0x9f19, 0x6068, 0x47f2,
            0xc8db, 0x8025, 0x3e28, 0xf0b2, 0xbad1, 0x1237, 0x3b1d, 0xe2fc, 0x24b7, 0xb8b8, 0x4d82,
            0x5adc, 0x16b4, 0x1bb7, 0xedec, 0x9f94, 0x3557, 0x4ce4, 0x9995, 0xec62, 0xce8e, 0x597e,
            0x0161, 0x12f7, 0xa4d3, 0x98c7, 0xaede, 0x7e2d, 0xaa32, 0x98e4, 0xbfd7, 0x7e5a, 0x9507,
            0x8900, 0x1f5a, 0x46f5, 0x64cf, 0x6885, 0x6977, 0x26c4, 0xd94a, 0xe454, 0xcd75, 0xeda1,
            0x476b, 0x697c, 0xe522, 0x4ab9, 0x9e88, 0xde52, 0x67e4, 0xb170, 0x3270, 0x6291, 0x2422,
            0x95bb, 0xcf27, 0x90da, 0x12b2, 0x1305, 0x029b, 0x8427, 0x52e5, 0x3e64, 0x7a88, 0xd34d,
            0x68ee, 0x6099, 0xae6d, 0x622f, 0x1237, 0x33bd, 0x0143, 0x1e1c, 0xd463, 0xda74, 0x7272,
            0xa794, 0x1714, 0x8ec6, 0xf919, 0xdb4c, 0x60d7, 0xa3ae, 0xe336, 0x12bf, 0xc469, 0xfc67,
            0x9037, 0xcb6a, 0x5ebd, 0x85b5, 0x6c11, 0xa54e, 0x7e7f, 0xec0d, 0x46e5, 0x43ec, 0x6bf5,
            0x086f, 0x9421, 0xf5f7, 0xdbdf, 0x9994, 0x072c, 0xe5d9, 0x19a5, 0x8458, 0xec68, 0xba3f,
            0x9924,
        ];
        let lt_hash_hello = get_lt_hash("hello".as_bytes());
        assert_eq!(AsRef::<[u16]>::as_ref(&lt_hash_hello), &LTHASH_HELLO);

        // lt hash for "world!"
        const LTHASH_WORLD: [u16; 1024] = [
            0x56dc, 0x1d98, 0x5420, 0x810d, 0x936f, 0x1011, 0xa2ff, 0x6681, 0x637e, 0x9f2c, 0x0024,
            0xebd4, 0xe5f2, 0x3382, 0xd48b, 0x209e, 0xb031, 0xe7a5, 0x026f, 0x55f1, 0xc0cf, 0xe566,
            0x9eb0, 0x0a41, 0x3eb1, 0x3d36, 0x1b7c, 0x83ca, 0x9aa6, 0x2264, 0x8794, 0xfb85, 0x71e0,
            0x64c9, 0x227c, 0xed27, 0x09e0, 0xe5d5, 0xc8da, 0x88a5, 0x8b49, 0xf5a5, 0x3137, 0xbeed,
            0xca0e, 0x7690, 0x0570, 0xa5de, 0x4e0b, 0x4827, 0x4ae4, 0x2dad, 0x0ce4, 0xd56f, 0x9819,
            0x5d4e, 0xe93a, 0x0024, 0xb7b2, 0xc7ba, 0xa00c, 0x6709, 0x1d26, 0x53d3, 0x17b1, 0xebdf,
            0xb18f, 0xb30a, 0x3d6b, 0x1d75, 0x26a0, 0x260e, 0x6585, 0x2ba6, 0xc88d, 0x70ef, 0xf6f4,
            0x8b7f, 0xc03b, 0x285b, 0x997b, 0x933e, 0xf139, 0xe097, 0x3eff, 0xd9f7, 0x605a, 0xaeec,
            0xee8d, 0x1527, 0x3bff, 0x7081, 0xda28, 0x4c0f, 0x44b0, 0xb7d0, 0x8f9b, 0xa657, 0x8e47,
            0xa405, 0x5507, 0xe5f9, 0x52ed, 0xc4e1, 0x300c, 0x0db3, 0xbf93, 0xfddd, 0x8f21, 0x10c5,
            0x4bfd, 0x5f13, 0xe136, 0xd72f, 0x1822, 0xb424, 0x996f, 0x8fdd, 0x0703, 0xa57f, 0x7923,
            0x0755, 0x7aee, 0x168d, 0x1525, 0xf912, 0xb48d, 0xfb9e, 0xd606, 0xb2ce, 0x98ef, 0x20fb,
            0xd21a, 0x8261, 0xd6db, 0x61bf, 0xdbc6, 0x02b1, 0x45e9, 0x1ffa, 0x071f, 0xa2c0, 0x74a8,
            0xae54, 0x59e1, 0xe2dc, 0x0ec9, 0x35ac, 0xbbb0, 0x5938, 0x2210, 0xcf9e, 0x2d9f, 0x7e01,
            0x2ab7, 0xd7d8, 0x8e36, 0x6b09, 0x262c, 0xb017, 0x9b6e, 0x1455, 0x7401, 0x8a8a, 0x6491,
            0x9de9, 0x7856, 0x8fb3, 0x8fcb, 0x3c05, 0x3e74, 0x40a4, 0x682a, 0x1a67, 0x9888, 0xb949,
            0xbb75, 0x6ef9, 0xc457, 0xa83a, 0x7965, 0x159e, 0xa415, 0x1c6b, 0x1b94, 0xaa10, 0x137d,
            0xbc3a, 0xc6bd, 0xf303, 0x7758, 0xc8da, 0xf5a3, 0x5826, 0x2b48, 0x9852, 0x3033, 0xfa85,
            0x3f85, 0x9b38, 0xd409, 0x4813, 0x36b2, 0x43d7, 0xdc0a, 0xfb54, 0x22b2, 0xf1e1, 0xfe5a,
            0x44ff, 0x217c, 0x158d, 0x2041, 0x7d2a, 0x4a78, 0xfc39, 0xb7db, 0x4786, 0xf8ee, 0xc353,
            0x96c2, 0x7be2, 0xd18d, 0x0407, 0x7b0e, 0x04f5, 0x3c63, 0x415e, 0xb1d1, 0x31cc, 0x25ac,
            0x9d8a, 0x4845, 0xd2b4, 0x0cdd, 0xf9a4, 0xae8f, 0x7fe5, 0x2285, 0xa749, 0x43cb, 0x16ae,
            0x09a9, 0xbd32, 0x923c, 0x2825, 0xbe21, 0xfa66, 0x2638, 0x3435, 0x6d79, 0xdf4b, 0xaab4,
            0xf2b1, 0x08f4, 0x64fd, 0x7364, 0x14e4, 0x1457, 0xbce3, 0xe114, 0xeccb, 0x2490, 0xae79,
            0x7448, 0x6310, 0xeff6, 0x2bb1, 0x79e7, 0xf5ae, 0xab40, 0xff6d, 0x889b, 0xe5f5, 0x69ee,
            0x3298, 0x512a, 0x2573, 0xf85c, 0xc69a, 0xb142, 0x3ed0, 0x7b9d, 0xc7a5, 0xea5d, 0xd085,
            0x4e99, 0xaf95, 0x404b, 0x8aca, 0x870f, 0x098a, 0x7c9c, 0x30cf, 0x3e16, 0x9010, 0xa94b,
            0x3cca, 0x00bc, 0xddb8, 0xbf1b, 0xc61a, 0x7121, 0xd668, 0xf4ba, 0xb339, 0xa66c, 0xd5b9,
            0x557c, 0x70a0, 0x34e4, 0x43a5, 0x9c32, 0x2e94, 0xa47f, 0x0b21, 0xb594, 0xb483, 0xf823,
            0x8c56, 0x9ee9, 0x71aa, 0xf97c, 0x1c62, 0xe003, 0xcbbe, 0xca8f, 0x58e5, 0xcbee, 0x758e,
            0x5511, 0x38da, 0x7816, 0xd6a1, 0x4550, 0x09e9, 0x682f, 0xf2ca, 0x5ea1, 0x58c2, 0x78ed,
            0xb630, 0xee80, 0xa2df, 0xa890, 0x8b42, 0x83d0, 0x7ec6, 0xa87e, 0x896c, 0xf649, 0x173d,
            0x4950, 0x5d0a, 0xd1a8, 0x7376, 0x4a4a, 0xe53f, 0x447d, 0x6efd, 0xd202, 0x1da3, 0x4825,
            0xd44b, 0x4343, 0xa1a9, 0x8aac, 0x5b50, 0xc8e6, 0x8086, 0xd64f, 0xd077, 0x76f0, 0x9443,
            0xcd70, 0x950d, 0x0369, 0xf1be, 0xb771, 0x5222, 0x4b40, 0x4846, 0x3fab, 0x1d5d, 0xc69d,
            0xa200, 0xe217, 0xb8bd, 0x2ef7, 0xed6b, 0xa78c, 0xe978, 0x0e16, 0x72bf, 0x05a3, 0xdcb4,
            0x4024, 0xfca2, 0x0219, 0x0d3e, 0xa83f, 0x6127, 0x33ab, 0x3ae5, 0xe7a1, 0x2e76, 0xf6f5,
            0xbee1, 0xa712, 0xab89, 0xf058, 0x71ed, 0xd39e, 0xa383, 0x5f64, 0xe2b6, 0xbe86, 0xee47,
            0x5bd8, 0x1536, 0xc6ed, 0x1c40, 0x836d, 0xcc40, 0x18ff, 0xe30a, 0xae2c, 0xc709, 0x7b40,
            0xddf8, 0x7b72, 0x97da, 0x3f71, 0x6dba, 0x578b, 0x980a, 0x2e0e, 0xd0c0, 0x871f, 0xde9b,
            0xa821, 0x1a41, 0xbff0, 0x04cb, 0x40d6, 0x9942, 0xf717, 0x2c1a, 0x65f9, 0xae3d, 0x9e4e,
            0x3ca6, 0x2d53, 0x3f6e, 0xc886, 0x5bbc, 0x9936, 0x09de, 0xb4ab, 0xc044, 0xa7a0, 0x8c37,
            0x383a, 0x3ab9, 0xcd16, 0x33c2, 0x908e, 0x75c3, 0x51da, 0xcb86, 0x4640, 0xe2b7, 0xbc2f,
            0x1bbb, 0xc1c0, 0xc4ce, 0x821d, 0x0a46, 0x178c, 0x1291, 0xfe6e, 0xd15f, 0x8d3e, 0x9d01,
            0x79b2, 0xfe4c, 0x75eb, 0x176c, 0x6be7, 0x6efa, 0xdcc6, 0x2127, 0xef2b, 0xb83a, 0xe10b,
            0x3206, 0xc2fe, 0x1a3d, 0x62c8, 0xf55e, 0xc594, 0x81ba, 0x0188, 0x962a, 0x0f1c, 0x2489,
            0xb3ca, 0x0d9a, 0xca06, 0xfe37, 0x2cb0, 0x87a1, 0xd33b, 0x31b0, 0x1efe, 0x08f2, 0xc55a,
            0xcb8a, 0x1633, 0x9df2, 0xc468, 0xd5e3, 0x3117, 0x3333, 0x488f, 0x4a9d, 0xc68f, 0x73f9,
            0xa82d, 0xe1af, 0xeb4e, 0xe41b, 0x33f5, 0x051f, 0x7592, 0x0528, 0x7aee, 0xc3eb, 0x7010,
            0x03f4, 0xaba4, 0x3e8f, 0x4abd, 0x2b41, 0x5390, 0x21a1, 0x6dc6, 0xd828, 0xa9b4, 0xc63a,
            0x3ab3, 0x14aa, 0xdc3a, 0x513f, 0x9886, 0x0000, 0x1169, 0xbba0, 0xb2fe, 0x4b09, 0x0198,
            0xcfff, 0xb898, 0x8cfe, 0x3def, 0x0b4b, 0xc154, 0x2491, 0x28d7, 0x757f, 0x06c5, 0x98c5,
            0x2dfa, 0xc068, 0xc74d, 0x521e, 0x70d5, 0xde35, 0x7718, 0xddf8, 0xa387, 0x807d, 0x0056,
            0x697b, 0x3043, 0x4ec8, 0xc2be, 0xa867, 0x0555, 0x2d3f, 0xc9f1, 0xfe7c, 0xe851, 0x5b85,
            0x2175, 0x741d, 0x1e5b, 0xafd3, 0xf757, 0x1bd9, 0x96df, 0x03df, 0x28d6, 0xbb77, 0xd5b5,
            0x03d3, 0xc078, 0x255b, 0xee39, 0x9705, 0x7fcc, 0xf16e, 0x16ca, 0x71d1, 0x9107, 0x00a5,
            0x103d, 0x0b12, 0xea24, 0xdf09, 0x7745, 0x7c1b, 0xcdba, 0x3093, 0x742e, 0x1e4c, 0x087b,
            0x9661, 0x0f3a, 0x6c51, 0xdc63, 0xb9d8, 0xf518, 0x09e1, 0x1426, 0xb6dc, 0xc246, 0xa273,
            0x5562, 0x8fde, 0x8f0e, 0xd034, 0x6651, 0x95ec, 0x6452, 0x95d4, 0xdf84, 0x118c, 0x44ab,
            0x328b, 0xf3d1, 0xb048, 0x2081, 0x748a, 0x05ee, 0x0f9b, 0x8110, 0x46e8, 0x6476, 0x8863,
            0x9850, 0xcb94, 0x2d2e, 0xcbac, 0xce53, 0x91bb, 0xa605, 0xfe50, 0x06f5, 0xef2d, 0xbd7c,
            0x736b, 0xf371, 0x6055, 0x6ab9, 0x135f, 0xb572, 0x5eb1, 0x7a36, 0xe4d5, 0xb998, 0xa7ea,
            0x1d06, 0x1275, 0x7f89, 0x3c92, 0xe906, 0x40c1, 0x8207, 0x058e, 0xa660, 0x72cd, 0xce25,
            0xd92a, 0x7731, 0x7633, 0xc6da, 0xb213, 0x0a93, 0x30c0, 0x58d3, 0x5ac0, 0x3ce7, 0x1028,
            0x4bcd, 0x86b9, 0x7f60, 0x22a6, 0x0ce9, 0xb569, 0x8c83, 0xb5bf, 0x2dd9, 0x7bdd, 0xc4bc,
            0xce57, 0x0b0b, 0x0a9c, 0xd74a, 0x6936, 0x0e40, 0xa874, 0x02b2, 0xfe8d, 0x0c16, 0xa0e0,
            0x5b01, 0x6f18, 0x6264, 0x4e77, 0x01a0, 0x3484, 0xe5b4, 0xf0cc, 0xd30d, 0x7904, 0x8216,
            0x46dd, 0x6fc0, 0xfa77, 0x8c3e, 0x5c10, 0xf776, 0x3043, 0x23dc, 0xfffc, 0x35c0, 0x8007,
            0x7993, 0xf198, 0x94eb, 0xe9bf, 0x7cc0, 0x170d, 0xea0d, 0xa7d0, 0x3d77, 0x7d6e, 0xc8f7,
            0x9a86, 0x6462, 0xc8d2, 0x357a, 0x8fa0, 0xf201, 0x55e5, 0x5235, 0x7da1, 0x52e6, 0xcc31,
            0xbecd, 0x3343, 0x343a, 0x2b1f, 0xd19e, 0x4cc6, 0x83a2, 0x6d16, 0x9c97, 0xa61b, 0xde54,
            0x6da1, 0xa57e, 0x44a7, 0x1e84, 0x98e7, 0x0e44, 0x5494, 0xe013, 0x0ed2, 0x0b3a, 0xa2db,
            0xc93a, 0xe6a0, 0xdccd, 0x84ac, 0xc898, 0xb974, 0x3d62, 0xe4cf, 0xcbc3, 0xa7bd, 0xde59,
            0x9391, 0x5635, 0xdac1, 0xd9b6, 0x1700, 0x7b35, 0x9555, 0x648e, 0xdacd, 0xffdf, 0xdd6a,
            0x9616, 0xea2e, 0xb1a4, 0x80c1, 0xdb21, 0x1076, 0x9543, 0xc165, 0x66d8, 0x26b8, 0x7095,
            0xdf4f, 0xcf4b, 0x1cec, 0xb231, 0x4037, 0x9fa5, 0x3637, 0xf96e, 0x215a, 0x65c9, 0x4696,
            0x734a, 0x556e, 0xb47f, 0x5160, 0xbf85, 0x850b, 0x06e0, 0x8181, 0x45f7, 0x202b, 0x86d1,
            0x5de7, 0x8ecd, 0xf77c, 0x031f, 0xa330, 0x79b4, 0xf38b, 0x59a8, 0x68cf, 0xf885, 0xfc87,
            0x4054, 0xe627, 0x845e, 0xa77f, 0x8450, 0x2302, 0x86e6, 0x2d94, 0xbbf7, 0x9e54, 0x2d79,
            0x1aa6, 0x6c50, 0xaef5, 0xbd9d, 0x85f3, 0x7b05, 0x5ec3, 0x6d70, 0x3ff3, 0x62a6, 0x252a,
            0x72c4, 0x2f56, 0xf9c1, 0xadf9, 0x00ff, 0xedfc, 0xddf3, 0x439c, 0x2777, 0xb742, 0xddfd,
            0x14fc, 0xa147, 0xd950, 0x37bd, 0x6296, 0xf816, 0x29af, 0x297c, 0xbf24, 0x6f05, 0xe8a4,
            0x17f4, 0xc8ab, 0xc0d1, 0x87b2, 0xeca2, 0x1b31, 0xa20b, 0xaad8, 0xd46c, 0x636f, 0x3975,
            0x363e, 0xdc79, 0xc450, 0x507e, 0xd8d5, 0x74c9, 0x56de, 0x92bc, 0x05eb, 0x749a, 0x3d98,
            0xf26a, 0x23fe, 0x4f29, 0x7856, 0x968c, 0x8794, 0x2835, 0x8dc3, 0xa440, 0x3b7b, 0xcc28,
            0x98e6, 0x36f1, 0xf305, 0x7641, 0xe895, 0x88d7, 0xedb3, 0x934a, 0x88c2, 0x0d19, 0xd558,
            0xe4bd, 0xe365, 0x5b52, 0xd26d, 0x77be, 0xe2cc, 0xd759, 0xb890, 0x5924, 0xf681, 0xfd5f,
            0xccf7, 0xc9b7, 0x544a, 0x1fe8, 0xacd1, 0x349e, 0xf889, 0x3e38, 0x980a, 0xfcf6, 0x4aaf,
            0xc970, 0x2699, 0xce48, 0x3229, 0x148e, 0x2c20, 0x28c1, 0x7fc3, 0x1cf6, 0x080c, 0x2f85,
            0x6ed0, 0xa884, 0xd958, 0xd555, 0x480d, 0x8874, 0xe8d4, 0x7c66, 0x226f, 0xbf4f, 0xbcea,
            0x3eeb, 0xac04, 0xc774, 0xbc95, 0xa97f, 0x8382, 0x165b, 0xc178, 0x708e, 0x8be5, 0x7eb4,
            0x84ad, 0x15d5, 0x5193, 0x4114, 0xd320, 0x9add, 0x85a3, 0x8b70, 0x1be3, 0xa39d, 0xbf82,
            0x6e04, 0x3bd2, 0xdf31, 0x0741, 0xaab8, 0xd398, 0x01f4, 0xdd3a, 0x2f9d, 0x2b55, 0x6811,
            0x171f,
        ];
        let lt_hash_world = get_lt_hash("world!".as_bytes());
        assert_eq!(AsRef::<[u16]>::as_ref(&lt_hash_world), &LTHASH_WORLD);

        // add "hello" and "world!"
        let mut expected_sum = [0_u16; LT_HASH_ELEMENT];
        for i in 0..LT_HASH_ELEMENT {
            expected_sum[i] = LTHASH_HELLO[i].wrapping_add(LTHASH_WORLD[i]);
        }
        let mut lt_hash_sum = AccountLTHash::default();
        lt_hash_sum.add(&lt_hash_hello);
        lt_hash_sum.add(&lt_hash_world);
        assert_eq!(lt_hash_sum.to_u16(), expected_sum);

        // sub "hello"
        let mut lt_hash = lt_hash_sum;
        lt_hash.sub(&lt_hash_hello);
        assert_eq!(lt_hash.to_u16(), LTHASH_WORLD);

        // sub "world"
        let mut lt_hash = lt_hash_sum;
        lt_hash.sub(&lt_hash_world);

        assert_eq!(lt_hash.to_u16(), LTHASH_HELLO);
    }

    #[test]
    fn test_lt_hash_finalize() {
        let get_lt_hash = |input: &[u8]| -> AccountLTHash {
            let mut hasher = blake3::Hasher::new();
            hasher.update(input);
            AccountLTHash::new_from_reader(hasher.finalize_xof())
        };

        let lt_hash_hello = get_lt_hash("hello".as_bytes());
        let final_hash = lt_hash_hello.finalize();
        let expected = Hash::from_str("6MmGXJNfa8JKF5XhtHuKEhnLmr38vcyqBCwEXCCFnRVN").unwrap();
        assert_eq!(final_hash.0, expected);
    }

    #[test]
    fn test_lt_hash_bs58_str() {
        let get_lt_hash = |input: &[u8]| -> AccountLTHash {
            let mut hasher = blake3::Hasher::new();
            hasher.update(input);
            AccountLTHash::new_from_reader(hasher.finalize_xof())
        };

        let lt_hash_hello = get_lt_hash("hello".as_bytes());
        let hash_base58_str = "Y6vmUpYzw5f3AF2b3pAhqqz7qanMsqmB55WeaajtPi5wsMtRRku8RCQSjvVo9kUk2H97ZksbcayVzqZHZ56jkgER2EkSyArAQTVzfYDmpc7DU2VcYEcRWMTbHMBB8mTZDggjKMMHEqsXKEqVyUbW3w8nzHSS2MdasjcA6nsnQHG4ofNXpjjDSXUfXBvysqsRshcCMsvKsFjrNwwDuMa7sVQpXXe9YpG4Sej59FHJS5ExVmZfk36hawMbU3CSnM87C3RiWXuu9cNHKdndw561sbHkWwb2bzgaKCfh47hiQXDGb7yg22vpgsEb4F6K44xT1jpQnhbGGtLbV2B72ca4tsJgszvmeucQWgGuRWvh84tpn7EuPmWJt2CPQT4kbpJSYQW2jihL19Ao5tW7HsHhsaKHypxq9J9tFa179g864aRjvrvitoyDZWJHsRzMuD4ZGrZEATaWsSWcbfLh6QzRQie4YHyTfpUbm7eJqu8ohqwMnA7V2crAw1RAw3dAz8BfHA8TLNzkcq6B4q1KpcSAnFCTgEHSuymwuggLGMuj6u2VboynNvj36A22C2EqXuV3CTT8RXeEufsUWJERQH7TcUU7h9rWJDzjrf2ZmtD7d4jtfBF1yM4Qkd21SWHPa8893EQDGuk65r3wU4minie7iLozy3v7kk6kxozr2Ym5cWVng5g28rjkaJtEiihwKkJUrJZwxaVZZiRqBih4dt2WzZ8cpU5y6zvvXgHLo9YG6sRPKTx3txMNJTbtvD3tNQHBtMamHrJqYFy1w9u5bTYWP21usxRMyRCDfhRmtYuynDNeCk5NDTfo92nZK2W48UZGgLf1kUNJ7hMg5MSArAgLLDmp4S3HDGFCTdZKXtLvvg1ytR5V339tdrKzYp85xYRs9C6xd3LK3veB8qWHnSyK7z1fYBgCtos8vTUscs2Z4aHnZ36eof6wEeQE8DDekuZL5Lea9g32t4XHMBCkFxW2hhQqJ5pv3KpAGWTUmgHULeN4PDZ4hLP5MQujKRRvz5CUsXUe2bdcYVCWuHpqJRyAMw4z5QXJpX5yW5kY9DxzsdCkAHSfHY3T6YVxT7PaJJJXChG8ybUHNXPW8Zu4ZPG77bR84FaKUD6LVMumrnbmqd8TKxca8LAJpBDK2YeBABYja5e6QKqJsehFk53XopopSScqVdSaZUi3WYSyiY2vPq23zCmYLq3iWjBvLt8JG321LJBfadSSspeKiT8jreqj5iQMyBYnXrS5JTW3FuEobKPmMyjJ9WpSML4HD65gc7BBCH4eefUKeNrVR6MWdohfVr8fHXbzxYUbWYkr1axVYdwuBx674jinrofMBAVwkDLegLSMbkpJ3U5pUQRTYfe6fFH7iPguBxQGmHa7dP4FHmH3x6Y3Mx3Ua3undWi1jf2f4geyn4rD7pTh97XwYU3uMoDZ329CeCdhFVphJLc6Jqx2v31pFBPqKmASi4iAZGPayK2mG4J5HFHrkRozjBbQGyGqpxpR9gfgQ1EZvao4zWjhoj4wXreziCvpqhDnwbKeQ7F1fLHbM3WsySXYs7PAiNFQAgD2oDTgpWZps9CobvFzs1GMjEYUn9cBXsmsx4UjfUT3vwx4xV38wfcq8mpz98nciwA1hnNAzBCd2CtueDQvooojKq5kbBmY1p9gCxju1wuiae58qAfg9x9NqJtViSyqdF3VKYSCkMVoFRJTKFuYNvUxcEWG1pLy8ofiiWoN2obPYaKxrab8jDNtWyWVKizh6skZLZGFsTrpecTkc5ULC2kbYmf3UgyALZAKoeLoFkiQQ4E9YMmGdsYrGQBnPnrGYFjjgajCLurnYgEYQuxofFutPCk3XzWJXdjFTQMoA9LtFc47wKiJtH3uZernWMoAjy1BgM7jFbiLN1muJRnLsnUa6vKwLfk8hbYn8SfN63N2CkqdhEeJ8dEKLvu741HfkX2gi8rWWR5oKWQA9NQrPK4zBttBJZHuWCsh1M9FAXj5HJCdCyZFJUdCiP2ebcMY6rrAkVbH8tyWMFCKnguaeeYF9nD9DPbjLgsYURnAhWprwNCJKe5sKrPBqD4FZVxwS8F3sctCBchMrpocj6Hhe1UtV4ke9q92pb6HyFZ8LRFQDZjuSEqGzaSP5w9cuuvckz4xDRzqFhrrG5ZfJovBDrz9hBiPkVFaZs7NfrdRaZGkurerqwk1gBT87JDk6UY6ZRAyEZQDdfVvPBqsbHSxB2BQZTWPvJqvqZE8K9dobd5A5WU6Rcxp9tBwANL7ouCNTFs9JzbAgghDMZUMHvqBNmQArfYBidLku1fjepUmVfV4YzfaAHFuKrk5NbcfACYHG8RHoAqBz24d5oUJacP354eHNWu1hYWbH73metXHqadrgk3YtQHKaK3bmZnxd4AVPVy916GfnfQoctKhXbTaJmz1kQVebCpX8xpCEdyrXStD4XzrxtrVzh3EsgRfonReZEmSRkRa7B3Wv7e8ZX2JcMYQbVddyiiVRD7apcNbFo9qfQpHd15wF51AE1P7D2HtoNtJUa5f3a3UiN6rXLE83ek57zDiSyDEwHsCdgWTeL92zvxxFiD1vewLdtfoZcmDAvxuVcFyeEEYBLQRPLoKxRoxvjKjqzrEvi9WXH89k986ufxZTSefgcYNz8udNeoejZEgeq9dp3VNF8stsKK5bP9wKg9vEiXk536Mqo622e1gJozYunB4C9fz4ZczS6gDemjpohPvs5nX39zgBN89RJ6apvJN9JgvMnxCpR93LCEVFMUXoBNwGRpJcPf4XZd3Rz5kedCZem5WwPH2cYZGC";
        let hash_from_str = AccountLTHash::from_str(hash_base58_str).unwrap();
        assert_eq!(lt_hash_hello, hash_from_str);

        let mut hash_base58_string = hash_base58_str.to_string();
        hash_base58_string.push_str(hash_base58_str);
        assert_eq!(
            AccountLTHash::from_str(&hash_base58_string),
            Err(ParseLTHashError::WrongSize)
        );

        hash_base58_string.truncate(hash_base58_string.len() / 2);
        assert!(AccountLTHash::from_str(&hash_base58_string).is_ok());

        hash_base58_string.truncate(hash_base58_str.len() / 2);
        assert_eq!(
            AccountLTHash::from_str(&hash_base58_string),
            Err(ParseLTHashError::WrongSize)
        );

        let mut hash_base58_string = hash_base58_str.to_string();
        hash_base58_string.replace_range(..1, "O");
        assert_eq!(
            AccountLTHash::from_str(&hash_base58_string),
            Err(ParseLTHashError::Invalid)
        );
    }

    #[test]
    #[should_panic]
    fn test_lt_hash_new_size_too_big() {
        let slice = [1; LT_HASH_BYTES - 1];
        let _h = AccountLTHash::new(&slice);
    }

    #[test]
    #[should_panic]
    fn test_lt_hash_new_size_too_small() {
        let slice = [1; LT_HASH_BYTES - 1];
        let _h = AccountLTHash::new(&slice);
    }

    #[test]
    fn test_lt_hash_new() {
        let slice = [1; LT_HASH_BYTES];
        let _h = AccountLTHash::new(&slice);
    }
}
