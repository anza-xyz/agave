use {
    super::{
        bucket_map_holder::BucketMapHolder, in_mem_accounts_index::InMemAccountsIndex,
        AccountsIndexConfig, DiskIndexValue, IndexValue, Startup,
    },
    crate::{accounts_index, waitable_condvar::WaitableCondvar},
    std::{
        fmt::Debug,
        num::NonZeroUsize,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{Builder, JoinHandle},
    },
};

/// Manages the lifetime of the background processing threads.
pub struct AccountsIndexStorage<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> {
    _bg_threads: BgThreads,

    pub storage: Arc<BucketMapHolder<T, U>>,
    pub in_mem: Box<[Arc<InMemAccountsIndex<T, U>>]>,
}

impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> Debug for AccountsIndexStorage<T, U> {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

/// low-level managing the bg threads
struct BgThreads {
    exit: Arc<AtomicBool>,
    handles: Option<Vec<JoinHandle<()>>>,
    wait: Arc<WaitableCondvar>,
}

impl Drop for BgThreads {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        self.wait.notify_all();
        if let Some(handles) = self.handles.take() {
            handles
                .into_iter()
                .for_each(|handle| handle.join().unwrap());
        }
    }
}

impl BgThreads {
    fn new<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>(
        storage: &Arc<BucketMapHolder<T, U>>,
        in_mem: &[Arc<InMemAccountsIndex<T, U>>],
        threads: NonZeroUsize,
        can_advance_age: bool,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let is_disk_index_enabled = storage.is_disk_index_enabled();
        let num_threads = if is_disk_index_enabled {
            threads.get()
        } else {
            // no disk index, so only need 1 thread to report stats
            1
        };

        // stop signal used for THIS batch of bg threads
        let local_exit = Arc::new(AtomicBool::default());
        let handles = Some(
            (0..num_threads)
                .map(|idx| {
                    // the first thread we start is special
                    let can_advance_age = can_advance_age && idx == 0;
                    let storage_ = Arc::clone(storage);
                    let local_exit = local_exit.clone();
                    let system_exit = exit.clone();
                    let in_mem_ = in_mem.to_vec();

                    // note that using rayon here causes us to exhaust # rayon threads and many tests running in parallel deadlock
                    Builder::new()
                        .name(format!("solIdxFlusher{idx:02}"))
                        .spawn(move || {
                            storage_.background(
                                vec![local_exit, system_exit],
                                in_mem_,
                                can_advance_age,
                            );
                        })
                        .unwrap()
                })
                .collect(),
        );

        BgThreads {
            exit: local_exit,
            handles,
            wait: Arc::clone(&storage.wait_dirty_or_aged),
        }
    }
}

impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> AccountsIndexStorage<T, U> {
    /// startup=true causes:
    ///      in mem to act in a way that flushes to disk asap
    /// startup=false is 'normal' operation
    pub(crate) fn set_startup(&self, startup: Startup) {
        let is_startup = startup != Startup::Normal;
        self.storage.set_startup(is_startup);
    }

    /// estimate how many items are still needing to be flushed to the disk cache.
    pub fn get_startup_remaining_items_to_flush_estimate(&self) -> usize {
        self.storage
            .disk
            .as_ref()
            .map(|_| self.storage.stats.get_remaining_items_to_flush_estimate())
            .unwrap_or_default()
    }

    /// allocate BucketMapHolder and InMemAccountsIndex[]
    pub fn new(bins: usize, config: &AccountsIndexConfig, exit: Arc<AtomicBool>) -> Self {
        let num_flush_threads = config
            .num_flush_threads
            .unwrap_or_else(accounts_index::default_num_flush_threads);

        let storage = Arc::new(BucketMapHolder::new(bins, config, num_flush_threads.get()));

        let num_initial_accounts = config.num_initial_accounts;
        let in_mem: Box<_> = (0..bins)
            .map(|bin| Arc::new(InMemAccountsIndex::new(&storage, bin, num_initial_accounts)))
            .collect();

        Self {
            _bg_threads: BgThreads::new(&storage, &in_mem, num_flush_threads, true, exit),
            storage,
            in_mem,
        }
    }
}
