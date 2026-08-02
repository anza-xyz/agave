use {
    super::Bank,
    crossbeam_utils::CachePadded,
    rayon::{
        ThreadPool, ThreadPoolBuilder,
        iter::{IntoParallelIterator, ParallelIterator},
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{accounts_db::AccountsDb, storable_accounts::StorableAccounts},
    solana_lattice_hash::lt_hash::LtHash,
    solana_pubkey::Pubkey,
    std::{
        array, hint,
        mem::size_of,
        sync::{
            Arc, LazyLock, Mutex,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
        time::Instant,
    },
};

/// Number of threads for the async accounts hasher thread pool.
const NUM_ACCOUNTS_HASHER_THREADS: usize = 4;

// Maximum size, in bytes, for the seen-accounts freelist.
const MAX_BYTES_SEEN_ACCOUNTS_FREELIST: usize = 10_000_000;

impl Bank {
    /// Enqueues the accounts lt hash updates for `accounts` to the accounts hasher thread pool.
    ///
    /// This fn is meant to be called by on-chain events, e.g. transaction processing.
    /// This fn deduplicates from `accounts`, keeping only the latest version of each account.
    /// It also loads the previous version of each account inline, because we assume the previous
    /// version of each account is still in the accounts write cache, and thus fast to load.
    ///
    /// For non-transaction processing callers, consider `enqueue_off_chain_accounts_lt_hash_updates()`.
    pub fn enqueue_on_chain_accounts_lt_hash_updates<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
    ) {
        if accounts.is_empty() {
            return;
        }

        let seen_accounts_freelist = seen_accounts_freelist();
        let mut seen_accounts = seen_accounts_freelist.try_pop().unwrap_or_default();
        let async_progress = &self.accounts_lt_hash_async_progress;
        let thread_pool = accounts_hasher_thread_pool();

        // process accounts in reverse because we must only count the latest version of each account
        for index in (0..accounts.len()).rev() {
            let address = accounts.pubkey(index);
            if !seen_accounts.insert(*address) {
                // we've already enqueued a newer update for the same account; skip this one
                continue;
            }
            let prev_account = self
                .rc
                .accounts
                .load_with_fixed_root_do_not_populate_read_cache(&self.ancestors, address)
                .map(|(account, _slot)| account);
            let curr_account = accounts.account(index, |account| {
                (account.lamports() != 0).then(|| account.take_account())
            });
            if prev_account.is_none() && curr_account.is_none() {
                // the account was ephemeral; skip it
            } else {
                // the account was modified; enqueue this update
                async_progress.spawn(
                    thread_pool,
                    AccountsLtHashUpdate {
                        address: *address,
                        prev_account,
                        curr_account,
                    },
                );
            }
        }

        // reclaim the seen accounts hashset
        seen_accounts_freelist.try_push(seen_accounts);
    }

    /// Enqueues the accounts lt hash updates for `accounts` to the accounts hasher thread pool.
    ///
    /// This fn is meant to be called by off-chain events, meaning we know/control `accounts`.
    /// Contrasting with `enqueue_on_chain_accounts_lt_hash_updates()`, this fn:
    /// - Does not deduplicate accounts, requiring the caller to ensure there are no duplicates.
    /// - Does not assume loading the previous version of accounts is fast,
    ///   e.g. when storing stake accounts as part of partitioned epoch rewards.
    ///
    /// If Some, `thread_pool_for_hashing_accounts` will be used
    /// to load the previous version of accounts in parallel.
    pub fn enqueue_off_chain_accounts_lt_hash_updates<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
        thread_pool_for_loading_accounts: Option<&ThreadPool>,
    ) {
        if cfg!(debug_assertions) {
            // if debug assertions are on, we will check for duplicates
            use ahash::HashSetExt as _;
            let mut seen_accounts = ahash::HashSet::with_capacity(accounts.len());
            let mut duplicate_pubkeys = ahash::HashSet::with_capacity(0); // assume no duplicates
            for index in 0..accounts.len() {
                let pubkey = accounts.pubkey(index);
                if !seen_accounts.insert(pubkey) {
                    // we've already seen this account, so add it to the duplicates list
                    duplicate_pubkeys.insert(pubkey);
                }
            }
            if !duplicate_pubkeys.is_empty() {
                let mut duplicate_accounts = ahash::HashMap::<_, Vec<_>>::default();
                for duplicate_pubkey in duplicate_pubkeys {
                    for index in 0..accounts.len() {
                        let pubkey = accounts.pubkey(index);
                        if pubkey == duplicate_pubkey {
                            duplicate_accounts
                                .entry(pubkey)
                                .or_default()
                                .push(accounts.account(index, |account| account.take_account()));
                        }
                    }
                }
                panic!("duplicate accounts were enqueued for hashing: {duplicate_accounts:?}");
            }
        }

        let async_progress = &self.accounts_lt_hash_async_progress;
        let thread_pool_for_hashing_accounts = accounts_hasher_thread_pool();

        // A closure that does the loading and enqueueing, so code is shared
        // whether using the thread_pool_for_loading_accounts or not.
        let load_then_enqueue = |index| {
            let address = accounts.pubkey(index);
            let prev_account = self
                .rc
                .accounts
                .load_with_fixed_root_do_not_populate_read_cache(&self.ancestors, address)
                .map(|(account, _slot)| account);
            let curr_account = accounts.account(index, |account| {
                (account.lamports() != 0).then(|| account.take_account())
            });
            if prev_account.is_none() && curr_account.is_none() {
                // the account was ephemeral; skip it
            } else {
                // the account was modified; enqueue this update
                async_progress.spawn(
                    thread_pool_for_hashing_accounts,
                    AccountsLtHashUpdate {
                        address: *address,
                        prev_account,
                        curr_account,
                    },
                );
            }
        };

        if let Some(thread_pool_for_loading_accounts) = thread_pool_for_loading_accounts {
            // The previous version of accounts must be loaded before subsequent account
            // modifications occur, so ThreadPool::spawn() canot be used here.
            thread_pool_for_loading_accounts.install(|| {
                (0..accounts.len())
                    .into_par_iter()
                    .for_each(load_then_enqueue);
            });
        } else {
            (0..accounts.len()).for_each(load_then_enqueue);
        }
    }

    /// Updates the accounts lt hash.
    ///
    /// When freezing a bank, we compute and update the accounts lt hash.
    /// For each account modified in this bank, we:
    /// - mix out its previous state, and
    /// - mix in its current state
    ///
    /// This function waits for any in-flight jobs on the accounts hasher threads,
    /// computes their combined delta lt hash, then mixes it into the bank.
    pub fn finish_accounts_lt_hash_updates(&self) {
        let timer = Instant::now();
        let num_jobs_total = {
            let mut accounts_lt_hash = self.accounts_lt_hash.lock().unwrap();
            self.accounts_lt_hash_async_progress
                .finish(&mut accounts_lt_hash.0)
        };
        let finish_time = timer.elapsed();

        let seen_accounts_freelist_stats = seen_accounts_freelist().stats();
        datapoint_info!(
            "bank-accounts_lt_hash",
            ("slot", self.slot(), i64),
            ("num_jobs", num_jobs_total, i64),
            ("finish_us", finish_time.as_micros(), i64),
            (
                "seen_accounts_freelist_num_containers",
                seen_accounts_freelist_stats.num_containers,
                i64
            ),
            (
                "seen_accounts_freelist_capacity_elems",
                seen_accounts_freelist_stats.capacity_elems,
                i64
            ),
            (
                "seen_accounts_freelist_capacity_bytes",
                seen_accounts_freelist_stats.capacity_bytes,
                i64
            ),
        );
    }
}

/// Struct for tracking progress of the asynchronous accounts lt hashing for a Bank.
pub struct AccountsLtHashAsyncProgress {
    // Note: use [Mutex<CachePadded<LtHash>>] and *not* [CachePadded<Mutex<LtHash>>].
    // - In both ways each mutex is on its own separate cache line.
    // - In both ways the size used for each element, including padding, is the same.
    // - Only this way ensures that each LtHash is placed for aligned SIMD/AVX access.
    //
    // Here's the layout of [Mutex<CachePadded<LtHash>>; 2]
    //
    //  │element 0                         │element 1
    //  │                                  │
    //  ▼───────┬─────────┬────────────────▼───────┬─────────┬────────────────┐
    //  │ Mutex │ padding │     LtHash     │ Mutex │ padding │     LtHash     │
    //  ├───────┼─────────┼────────────────┼───────┼─────────┼────────────────┤
    //  │       │         │                │       │         │                │
    //  │0      │6        │128 <-- aligned │2176   │2182     │2304            │4352
    //
    //
    // And here's the layout of [CachePadded<Mutex<LtHash>>; 2]
    //
    //  │element 0                         │element 1
    //  │                                  │
    //  ▼───────┬────────────────┬─────────▼───────┬────────────────┬─────────┐
    //  │ Mutex │     LtHash     │ padding │ Mutex │     LtHash     │ padding │
    //  ├───────┼────────────────┼─────────┼───────┼────────────────┼─────────┤
    //  │       │                │         │       │                │         │
    //  │0      │6 <-- unaligned │2054     │2176   │2182            │4230     │4352
    //
    accumulators: Arc<[Mutex<CachePadded<LtHash>>; NUM_ACCOUNTS_HASHER_THREADS]>,
    num_jobs_pending: Arc<AtomicUsize>,
    num_jobs_total: AtomicU64,
}

impl AccountsLtHashAsyncProgress {
    /// Creates a new AccountsLtHashAsyncProgress variable, which is suitable for a new Bank.
    pub fn new() -> Self {
        Self {
            accumulators: Arc::new(array::from_fn(|_| {
                Mutex::new(CachePadded::new(LtHash::identity()))
            })),
            num_jobs_pending: Arc::new(AtomicUsize::new(0)),
            num_jobs_total: AtomicU64::new(0),
        }
    }

    /// Enqueues `update` into `thread_pool` for asynchronous processing.
    fn spawn(&self, thread_pool: &'static ThreadPool, update: AccountsLtHashUpdate) {
        self.num_jobs_pending.fetch_add(1, Ordering::Relaxed);
        self.num_jobs_total.fetch_add(1, Ordering::Relaxed);
        thread_pool.spawn({
            let accumulators = Arc::clone(&self.accumulators);
            let num_jobs_pending = Arc::clone(&self.num_jobs_pending);
            move || {
                // SAFETY: We always call from the same/correct Rayon thread pool.
                let worker_index = thread_pool.current_thread_index().unwrap();

                // SAFETY: There are num_threads accumulators, and each
                // thread's index shall always be in range 0..num_threads.
                debug_assert!(worker_index < accumulators.len());
                let accumulator = unsafe { accumulators.get_unchecked(worker_index) };

                Self::process(&mut accumulator.lock().unwrap(), update);

                // Decrementing the number of pending jobs MUST happen *after*
                // accumulating the result.  This ensures `finish()` cannot
                // observe zero pending jobs until all workers are done.
                num_jobs_pending.fetch_sub(1, Ordering::Relaxed);
            }
        });
    }

    /// Waits for all pending jobs to complete, then mixes the results into `lt_hash`.
    ///
    /// Returns the number of asynchronous jobs completed.
    ///
    /// Note: Since an LtHash is large, `lt_hash` is passed as an in-out parameter.
    /// This it to avoid Rust compiler bug that fails to perform return value optimization.
    fn finish(&self, lt_hash: &mut LtHash) -> u64 {
        while self.num_jobs_pending.load(Ordering::Relaxed) > 0 {
            // Spin, do not yield! This is called by Bank::freeze() and we want to be fast.
            hint::spin_loop();
        }

        for thread_accumulator in self.accumulators.iter() {
            lt_hash.mix_in(&thread_accumulator.lock().unwrap());
        }
        self.num_jobs_total.load(Ordering::Relaxed)
    }

    /// Processes `update` and mixes the result into `accum_lt_hash`.
    ///
    /// Note: Since an LtHash is large, `accum_lt_hash` is passed as an in-out parameter.
    /// This it to avoid Rust compiler bug that fails to perform return value optimization.
    fn process(accum_lt_hash: &mut LtHash, update: AccountsLtHashUpdate) {
        let AccountsLtHashUpdate {
            address,
            prev_account,
            curr_account,
        } = update;
        if let Some(prev_account) = prev_account {
            let prev_lt_hash = AccountsDb::lt_hash_account(&prev_account, &address);
            accum_lt_hash.mix_out(&prev_lt_hash.0);
        }
        if let Some(curr_account) = curr_account {
            let curr_lt_hash = AccountsDb::lt_hash_account(&curr_account, &address);
            accum_lt_hash.mix_in(&curr_lt_hash.0);
        }
    }
}

/// A single accounts lt hash update to process.
#[derive(Debug)]
struct AccountsLtHashUpdate {
    address: Pubkey,
    prev_account: Option<AccountSharedData>,
    curr_account: Option<AccountSharedData>,
}

/// Get the freelist of hashsets to use for seen accounts.
fn seen_accounts_freelist() -> &'static HashSetFreelist<Pubkey> {
    // Derived empirically while observing an unstaked node on mnb.
    // Should end up being the same number as replay threads.
    const MAX_CONTAINERS: usize = 50;
    static FREELIST: LazyLock<HashSetFreelist<Pubkey>> = LazyLock::new(|| {
        HashSetFreelist::new(MAX_CONTAINERS, Some(MAX_BYTES_SEEN_ACCOUNTS_FREELIST))
    });
    &FREELIST
}

/// Freelist of containers, to avoid repeat allocations/deallocations.
#[derive(Debug)]
struct HashSetFreelist<T> {
    /// the maximum number of containers this freelist will hold
    max_containers: usize,

    /// the maximum capacity, in elements, this freelist will hold
    max_capacity: Option<usize>,

    inner: Mutex<HashSetFreelistInner<T>>,
}

impl<T> HashSetFreelist<T> {
    /// Creates a new, empty, freelist.
    ///
    /// max_containers:
    /// * The maximum number of containers this freelist can hold.
    ///
    /// max_bytes:
    /// * The maximum number of bytes this freelist can hold.
    /// * This value corresponds to the total capacity across all the containers in the freelist.
    /// * If `None`, there is no maximum.
    fn new(max_containers: usize, max_bytes: Option<usize>) -> Self {
        let max_capacity = max_bytes.map(|max_bytes| max_bytes / size_of::<T>());
        Self {
            max_containers,
            max_capacity,
            inner: Mutex::new(HashSetFreelistInner {
                list: Vec::with_capacity(max_containers),
                total_capacity: 0,
            }),
        }
    }

    /// Pushes `container` on to the freelist (IFF its capacity is greater than zero).
    fn try_push(&self, mut container: ahash::HashSet<T>) {
        // If the capacity is zero, then the container never allocated.
        // In that case, don't waste time putting it back into the freelist,
        // since there's nothing of value to reuse.
        //
        // Else, check if pushing the container would exceed the max capacity of the freelist.
        // If so, also do not put it back into the freelist.
        let capacity = container.capacity();
        if capacity == 0 {
            return;
        }

        // container must be empty to be reused, so do it here outside of the lock
        container.clear();

        let mut inner = self.inner.lock().unwrap();

        if inner.list.len() >= self.max_containers {
            // the num containers would exceed the max, do not push
            return;
        }

        let Some(new_total_capacity) = inner.total_capacity.checked_add(capacity) else {
            // the new total capacity would overflow, do not push
            return;
        };

        let max_capacity = self.max_capacity.unwrap_or(usize::MAX);
        if new_total_capacity > max_capacity {
            // the new total capacity would exceed the max, do not push
            return;
        }

        inner.list.push(container);
        inner.total_capacity = new_total_capacity;
    }

    /// Pops a container off the freelist and returns it.
    ///
    /// The returned container will always be empty.
    fn try_pop(&self) -> Option<ahash::HashSet<T>> {
        let mut inner = self.inner.lock().unwrap();
        let container = inner.list.pop()?;
        assert!(container.is_empty());
        inner.total_capacity -= container.capacity();
        Some(container)
    }

    /// Returns a snapshot of the freelist's stats.
    fn stats(&self) -> FreelistStats {
        let inner = self.inner.lock().unwrap();
        let num_containers = inner.list.len();
        let capacity_elems = inner.total_capacity;
        drop(inner);
        FreelistStats {
            num_containers,
            capacity_elems,
            capacity_bytes: capacity_elems.saturating_mul(size_of::<T>()),
        }
    }
}

/// The mutable state of a [`HashSetFreelist`], guarded by a single lock.
#[derive(Debug)]
struct HashSetFreelistInner<T> {
    /// the containers available for reuse
    list: Vec<ahash::HashSet<T>>,
    /// the capacity, in elements, across all the containers in the freelist
    total_capacity: usize,
}

/// A snapshot of a freelist's stats.
#[derive(Debug, Eq, PartialEq)]
struct FreelistStats {
    /// the number of containers held by the freelist
    num_containers: usize,
    /// the capacity, in elements, across all the containers in the freelist
    capacity_elems: usize,
    /// the capacity, in bytes, across all the containers in the freelist
    capacity_bytes: usize,
}

/// Returns the thread pool for asynchronous accounts hashing.
///
/// Note, the thread pool will be created on first call.
fn accounts_hasher_thread_pool() -> &'static ThreadPool {
    static THREAD_POOL: LazyLock<ThreadPool> = LazyLock::new(|| {
        ThreadPoolBuilder::new()
            .num_threads(NUM_ACCOUNTS_HASHER_THREADS)
            .thread_name(|i| format!("solAcctsHashr{i:02}"))
            .build()
            .expect("new accounts hasher rayon threadpool")
    });
    &THREAD_POOL
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            genesis_utils::create_genesis_config_with_leader_ex, runtime_config::RuntimeConfig,
            snapshot_bank_utils, snapshot_utils,
        },
        agave_feature_set::FeatureSet,
        agave_snapshots::snapshot_config::SnapshotConfig,
        ahash::HashSetExt as _,
        solana_accounts_db::{
            accounts_db::{ACCOUNTS_DB_CONFIG_FOR_TESTING, AccountsDbConfig},
            accounts_index::{ACCOUNTS_INDEX_CONFIG_FOR_TESTING, AccountsIndexConfig, IndexLimit},
        },
        solana_cluster_type::ClusterType,
        solana_fee_calculator::FeeRateGovernor,
        solana_genesis_config::{self, GenesisConfig},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_pubkey::{self as pubkey, Pubkey},
        solana_rent::Rent,
        solana_signer::Signer as _,
        std::{
            cmp, iter,
            str::FromStr as _,
            sync::{Arc, Barrier},
            thread,
        },
        tempfile::TempDir,
        test_case::{test_case, test_matrix},
    };

    /// What features should be enabled?
    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    enum Features {
        /// Do not enable any features
        None,
        /// Enable all features
        All,
    }

    /// Creates a genesis config with `features` enabled
    fn genesis_config_with(features: Features) -> (GenesisConfig, Keypair) {
        let mint_keypair = Keypair::new();
        let mint_lamports = 123_456_789 * LAMPORTS_PER_SOL;
        let validator_lamports = 100 * LAMPORTS_PER_SOL;
        let validator_stake_lamports = 10 * LAMPORTS_PER_SOL;
        let validator_pubkey = Pubkey::new_unique();
        let vote_account_pubkey = Pubkey::new_unique();
        let stake_account_pubkey = Pubkey::new_unique();
        let feature_set = match features {
            Features::None => FeatureSet::default(),
            Features::All => FeatureSet::all_enabled(),
        };

        let config = create_genesis_config_with_leader_ex(
            mint_lamports,
            &mint_keypair.pubkey(),
            &validator_pubkey,
            &vote_account_pubkey,
            &stake_account_pubkey,
            None,
            validator_stake_lamports,
            validator_lamports,
            FeeRateGovernor::default(),
            Rent::default(),
            ClusterType::Development,
            &feature_set,
            vec![],
        );

        (config, mint_keypair)
    }

    #[test]
    fn test_update_accounts_lt_hash() {
        // Write to address 1, 2, and 5 in first bank, so that in second bank we have
        // updates to these three accounts.  Make address 2 go to zero (dead).  Make address 1 and 3 stay
        // alive.  Make address 5 unchanged.  Ensure the updates are expected.
        //
        // 1: alive -> alive
        // 2: alive -> dead
        // 3: dead -> alive
        // 4. dead -> dead
        // 5. alive -> alive *unchanged*

        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();
        let keypair4 = Keypair::new();
        let keypair5 = Keypair::new();

        let (mut genesis_config, mint_keypair) =
            solana_genesis_config::create_genesis_config(123_456_789 * LAMPORTS_PER_SOL);
        // This test requires zero fees so that we can easily transfer an account's entire balance.
        genesis_config.fee_rate_governor = FeeRateGovernor::new(0, 0);
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // send lamports to accounts 1, 2, and 5 so they are alive,
        // and so we'll have a delta in the next bank
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &mint_keypair, &keypair1.pubkey())
            .unwrap();
        bank.transfer(amount, &mint_keypair, &keypair2.pubkey())
            .unwrap();
        bank.transfer(amount, &mint_keypair, &keypair5.pubkey())
            .unwrap();

        // manually freeze the bank to trigger updating the accounts lt hash
        bank.freeze();
        let prev_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // save the initial values of the accounts to use for asserts later
        let prev_mint = bank.get_account_with_fixed_root(&mint_keypair.pubkey());
        let prev_account1 = bank.get_account_with_fixed_root(&keypair1.pubkey());
        let prev_account2 = bank.get_account_with_fixed_root(&keypair2.pubkey());
        let prev_account3 = bank.get_account_with_fixed_root(&keypair3.pubkey());
        let prev_account4 = bank.get_account_with_fixed_root(&keypair4.pubkey());
        let prev_account5 = bank.get_account_with_fixed_root(&keypair5.pubkey());

        assert!(prev_mint.is_some());
        assert!(prev_account1.is_some());
        assert!(prev_account2.is_some());
        assert!(prev_account3.is_none());
        assert!(prev_account4.is_none());
        assert!(prev_account5.is_some());

        // These sysvars are also updated, but outside of transaction processing.  This means they
        // will not be in the accounts lt hash cache, but *will* be in the list of modified
        // accounts.  They must be included in the accounts lt hash.
        let sysvars = [
            Pubkey::from_str("SysvarS1otHashes111111111111111111111111111").unwrap(),
            Pubkey::from_str("SysvarC1ock11111111111111111111111111111111").unwrap(),
            Pubkey::from_str("SysvarRecentB1ockHashes11111111111111111111").unwrap(),
            Pubkey::from_str("SysvarS1otHistory11111111111111111111111111").unwrap(),
        ];
        let prev_sysvar_accounts: Vec<_> = sysvars
            .iter()
            .map(|address| bank.get_account_with_fixed_root(address))
            .collect();

        let bank = {
            let slot = bank.slot() + 1;
            Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), slot)
        };

        // send from account 2 to account 1; account 1 stays alive, account 2 ends up dead
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &keypair2, &keypair1.pubkey())
            .unwrap();

        // send lamports to account 4, then turn around and send them to account 3
        // account 3 will be alive, and account 4 will end dead
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &mint_keypair, &keypair4.pubkey())
            .unwrap();
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &keypair4, &keypair3.pubkey())
            .unwrap();

        // store account 5 into this new bank, unchanged
        bank.store_account(&keypair5.pubkey(), prev_account5.as_ref().unwrap());

        // freeze the bank to trigger updating the accounts lt hash
        bank.freeze();

        let post_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();
        let post_mint = bank.get_account_with_fixed_root(&mint_keypair.pubkey());
        let post_account1 = bank.get_account_with_fixed_root(&keypair1.pubkey());
        let post_account2 = bank.get_account_with_fixed_root(&keypair2.pubkey());
        let post_account3 = bank.get_account_with_fixed_root(&keypair3.pubkey());
        let post_account4 = bank.get_account_with_fixed_root(&keypair4.pubkey());
        let post_account5 = bank.get_account_with_fixed_root(&keypair5.pubkey());

        assert!(post_mint.is_some());
        assert!(post_account1.is_some());
        assert!(post_account2.is_none());
        assert!(post_account3.is_some());
        assert!(post_account4.is_none());
        assert!(post_account5.is_some());

        let post_sysvar_accounts: Vec<_> = sysvars
            .iter()
            .map(|address| bank.get_account_with_fixed_root(address))
            .collect();

        let mut expected_accounts_lt_hash = prev_accounts_lt_hash;
        let mut updater =
            |address: &Pubkey, prev: Option<AccountSharedData>, post: Option<AccountSharedData>| {
                // if there was an alive account, mix out
                if let Some(prev) = prev {
                    let prev_lt_hash = AccountsDb::lt_hash_account(&prev, address);
                    expected_accounts_lt_hash.0.mix_out(&prev_lt_hash.0);
                }

                // mix in the new one
                let post = post.unwrap_or_default();
                let post_lt_hash = AccountsDb::lt_hash_account(&post, address);
                expected_accounts_lt_hash.0.mix_in(&post_lt_hash.0);
            };
        updater(&mint_keypair.pubkey(), prev_mint, post_mint);
        updater(&keypair1.pubkey(), prev_account1, post_account1);
        updater(&keypair2.pubkey(), prev_account2, post_account2);
        updater(&keypair3.pubkey(), prev_account3, post_account3);
        updater(&keypair4.pubkey(), prev_account4, post_account4);
        updater(&keypair5.pubkey(), prev_account5, post_account5);
        for (i, sysvar) in sysvars.iter().enumerate() {
            updater(
                sysvar,
                prev_sysvar_accounts[i].clone(),
                post_sysvar_accounts[i].clone(),
            );
        }

        // now make sure the accounts lt hashes match
        let expected = expected_accounts_lt_hash.0.checksum();
        let actual = post_accounts_lt_hash.0.checksum();
        assert_eq!(
            expected, actual,
            "accounts_lt_hash, expected: {expected}, actual: {actual}",
        );
    }

    /// Ensure that the accounts lt hash is correct for slot 0
    ///
    /// This test does a simple transfer in slot 0 so that a primordial account is modified.
    ///
    /// Slot 0 is special because primordial accounts have no previous accounts lt hash entry.
    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_slot0_accounts_lt_hash(features: Features) {
        let (genesis_config, mint_keypair) = genesis_config_with(features);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        // ensure this bank is for slot 0, otherwise this test doesn't actually do anything...
        assert_eq!(bank.slot(), 0);

        // process a transaction that modifies a primordial account
        bank.transfer(LAMPORTS_PER_SOL, &mint_keypair, &Pubkey::new_unique())
            .unwrap();

        // manually freeze the bank to trigger updating the accounts lt hash
        bank.freeze();
        let actual_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // ensure the actual accounts lt hash matches the value calculated from the index
        let calculated_accounts_lt_hash = bank
            .rc
            .accounts
            .accounts_db
            .calculate_accounts_lt_hash_at_startup_from_index(&bank.ancestors);
        assert_eq!(actual_accounts_lt_hash, calculated_accounts_lt_hash);
    }

    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_calculate_accounts_lt_hash_at_startup_from_index(features: Features) {
        let (genesis_config, mint_keypair) = genesis_config_with(features);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // create some banks with some modified accounts so that there are stored accounts
        // (note: the number of banks and transfers are arbitrary)
        for _ in 0..7 {
            let slot = bank.slot() + 1;
            bank = Bank::new_from_parent_with_bank_forks(
                &bank_forks,
                bank,
                SlotLeader::default(),
                slot,
            );
            for _ in 0..13 {
                bank.register_unique_recent_blockhash_for_test();
                // note: use a random pubkey here to ensure accounts
                // are spread across all the index bins
                bank.transfer(amount, &mint_keypair, &pubkey::new_rand())
                    .unwrap();
            }
            bank.freeze();
        }
        let expected_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // root the bank and flush the accounts write cache to disk
        // (this more accurately simulates startup, where accounts are in storages on disk)
        bank.squash();
        bank.force_flush_accounts_cache();

        // call the fn that calculates the accounts lt hash at startup, then ensure it matches
        let calculated_accounts_lt_hash = bank
            .rc
            .accounts
            .accounts_db
            .calculate_accounts_lt_hash_at_startup_from_index(&bank.ancestors);
        assert_eq!(expected_accounts_lt_hash, calculated_accounts_lt_hash);
    }

    #[test_matrix(
        [Features::None, Features::All],
        [IndexLimit::Minimal, IndexLimit::InMemOnly]
    )]
    fn test_verify_accounts_lt_hash_at_startup(
        features: Features,
        accounts_index_limit: IndexLimit,
    ) {
        let (mut genesis_config, mint_keypair) = genesis_config_with(features);
        // This test requires zero fees so that we can easily transfer an account's entire balance.
        genesis_config.fee_rate_governor = FeeRateGovernor::new(0, 0);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // Write to this pubkey multiple times, so there are guaranteed duplicates in the storages.
        let duplicate_pubkey = pubkey::new_rand();

        // create some banks with some modified accounts so that there are stored accounts
        // (note: the number of banks and transfers are arbitrary)
        for _ in 0..9 {
            let slot = bank.slot() + 1;
            let leader = *bank.leader();
            bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, leader, slot);
            for _ in 0..3 {
                bank.register_unique_recent_blockhash_for_test();
                bank.transfer(amount, &mint_keypair, &pubkey::new_rand())
                    .unwrap();
                bank.register_unique_recent_blockhash_for_test();
                bank.transfer(amount, &mint_keypair, &duplicate_pubkey)
                    .unwrap();
            }

            // flush the write cache to disk to ensure there are duplicates across the storages
            bank.fill_bank_with_ticks_for_tests();
            bank.squash();
            bank.force_flush_accounts_cache();
        }

        // Create a few more storages to exercise the zero lamport duplicates handling during
        // generate_index(), which is used for the lattice-based accounts verification.
        // There needs to be accounts that only have a single duplicate (i.e. there are only two
        // versions of the accounts), and toggle between non-zero and zero lamports.
        // One account will go zero -> non-zero, and the other will go non-zero -> zero.
        let num_accounts = 2;
        let accounts: Vec<_> = iter::repeat_with(Keypair::new).take(num_accounts).collect();
        for i in 0..num_accounts {
            let slot = bank.slot() + 1;
            let leader = *bank.leader();
            bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, leader, slot);
            bank.register_unique_recent_blockhash_for_test();

            // transfer into the accounts so they start with a non-zero balance
            for account in &accounts {
                bank.transfer(amount, &mint_keypair, &account.pubkey())
                    .unwrap();
                assert_ne!(bank.get_balance(&account.pubkey()), 0);
            }

            // then transfer *out* all the lamports from one of 'em
            bank.transfer(
                bank.get_balance(&accounts[i].pubkey()),
                &accounts[i],
                &pubkey::new_rand(),
            )
            .unwrap();
            assert_eq!(bank.get_balance(&accounts[i].pubkey()), 0);

            // flush the write cache to disk to ensure the storages match the accounts written here
            bank.fill_bank_with_ticks_for_tests();
            bank.squash();
            bank.force_flush_accounts_cache();
        }
        bank.set_block_id(Some(Hash::default()));

        // verification happens at startup, so mimic the behavior by loading from a snapshot
        let bank_snapshots_dir = TempDir::new().unwrap();
        let snapshot_archives_dir = TempDir::new().unwrap();
        let snapshot_config = SnapshotConfig {
            full_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
            incremental_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
            bank_snapshots_dir: bank_snapshots_dir.path().to_path_buf(),
            ..SnapshotConfig::default()
        };
        let snapshot =
            snapshot_bank_utils::bank_to_full_snapshot_archive(&snapshot_config, &bank).unwrap();
        let (_accounts_tempdir, accounts_dir) = snapshot_utils::create_tmp_accounts_dir_for_tests();
        let accounts_index_config = AccountsIndexConfig {
            index_limit: accounts_index_limit,
            ..ACCOUNTS_INDEX_CONFIG_FOR_TESTING
        };
        let accounts_db_config = AccountsDbConfig {
            index: Some(accounts_index_config),
            ..ACCOUNTS_DB_CONFIG_FOR_TESTING
        };
        let roundtrip_bank = snapshot_bank_utils::bank_from_snapshot_archives(
            &[accounts_dir],
            &snapshot,
            None,
            &snapshot_config,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None, // leader_for_tests
            None,
            false,
            false,
            false,
            accounts_db_config,
            None,
            Arc::default(),
        )
        .unwrap();

        // Correctly calculating the accounts lt hash in Bank::new_from_snapshot() depends on the
        // bank being frozen.  This is so we don't call `update_accounts_lt_hash()` twice on the
        // same bank!
        assert!(roundtrip_bank.is_frozen());

        assert_eq!(roundtrip_bank, *bank);
    }

    /// Ensure that the snapshot hash is correct
    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_snapshots(features: Features) {
        let (genesis_config, mint_keypair) = genesis_config_with(features);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // create some banks with some modified accounts so that there are stored accounts
        // (note: the number of banks is arbitrary)
        for _ in 0..3 {
            let slot = bank.slot() + 1;
            let leader = *bank.leader();
            bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, leader, slot);
            bank.register_unique_recent_blockhash_for_test();
            bank.transfer(amount, &mint_keypair, &pubkey::new_rand())
                .unwrap();
            bank.fill_bank_with_ticks_for_tests();
            bank.squash();
            bank.force_flush_accounts_cache();
        }
        bank.set_block_id(Some(Hash::default()));

        let bank_snapshots_dir = TempDir::new().unwrap();
        let snapshot_archives_dir = TempDir::new().unwrap();
        let snapshot_config = SnapshotConfig {
            full_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
            incremental_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
            bank_snapshots_dir: bank_snapshots_dir.path().to_path_buf(),
            ..SnapshotConfig::default()
        };
        let snapshot =
            snapshot_bank_utils::bank_to_full_snapshot_archive(&snapshot_config, &bank).unwrap();
        let (_accounts_tempdir, accounts_dir) = snapshot_utils::create_tmp_accounts_dir_for_tests();
        let roundtrip_bank = snapshot_bank_utils::bank_from_snapshot_archives(
            &[accounts_dir],
            &snapshot,
            None,
            &snapshot_config,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None, // leader_for_tests
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();

        assert_eq!(roundtrip_bank, *bank);
    }

    /// Ensure enqueue_off_chain_accounts_lt_hash_updates() catches duplicates in debug mode.
    #[should_panic(expected = "duplicate accounts were enqueued for hashing")]
    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_enqueue_off_chain_accounts_lt_hash_updates_catches_duplicates(features: Features) {
        use rand::seq::SliceRandom as _;
        let (genesis_config, _) = genesis_config_with(features);
        let bank = Bank::new_for_tests(&genesis_config);

        let pubkey1 = pubkey::new_rand();
        let pubkey2 = pubkey::new_rand();
        let pubkey3 = pubkey::new_rand();

        let mut accounts = [
            // one version of pubkey1
            (&pubkey1, &AccountSharedData::new(11, 0, &Pubkey::default())),
            // two versions of pubkey2
            (&pubkey2, &AccountSharedData::new(21, 0, &Pubkey::default())),
            (&pubkey2, &AccountSharedData::new(22, 0, &Pubkey::default())),
            // three versions of pubkey3
            (&pubkey3, &AccountSharedData::new(31, 0, &Pubkey::default())),
            (&pubkey3, &AccountSharedData::new(32, 0, &Pubkey::default())),
            (&pubkey3, &AccountSharedData::new(33, 0, &Pubkey::default())),
        ];
        accounts.shuffle(&mut rand::rng());

        bank.store_accounts((bank.slot(), accounts.as_slice()), None);
    }

    /// Ensure freelist respects max size.
    #[test]
    fn test_freelist_max_capacity() {
        type Container = ahash::HashSet<u64>;

        // This test uses a hashbrown container, which has some special power-of-two sizing plus
        // a buffer.  So create the container first, and use that to derive the max capacity.
        let container = Container::with_capacity(77);

        let max_capacity = container.capacity();
        let max_bytes = max_capacity * size_of::<u64>();
        let mut freelist = HashSetFreelist::new(10, Some(max_bytes));

        // pushing a container that is too big will not actually push
        freelist.try_push(Container::with_capacity(max_capacity + 1));
        let stats0 = freelist.stats();
        assert_eq!(stats0.num_containers, 0);
        assert_eq!(stats0.capacity_elems, 0);
        assert_eq!(stats0.capacity_bytes, 0);

        // pushing a container that is not too big will actually push
        freelist.try_push(container);
        let stats1 = freelist.stats();
        assert_eq!(stats1.num_containers, 1);
        assert_eq!(stats1.capacity_elems, max_capacity);
        assert_eq!(stats1.capacity_bytes, max_bytes);

        // pushing a container that would exceed capacity will not push
        freelist.try_push(Container::with_capacity(1));
        assert_eq!(freelist.stats(), stats1);

        // ...but, if we remove the limit, push should work again
        freelist.max_capacity = None;
        let container = Container::with_capacity(1);
        let container_capacity = container.capacity();
        freelist.try_push(container);
        let stats2 = freelist.stats();
        assert_eq!(stats2.num_containers, 2);
        assert_eq!(stats2.capacity_elems, max_capacity + container_capacity);
        assert_eq!(
            stats2.capacity_bytes,
            max_bytes + container_capacity * size_of::<u64>(),
        );
    }

    /// Ensure concurrent pushes do not exceed the freelist's max capacity.
    #[test]
    fn test_freelist_concurrent_push() {
        // This test uses a hashbrown container, which has some special power-of-two sizing plus
        // a buffer.  So create the container first, and use that to derive the max capacity.
        let container = ahash::HashSet::<u64>::with_capacity(77);

        let num_threads = 16;
        let barrier = Arc::new(Barrier::new(num_threads));
        let max_capacity = container.capacity();
        let max_bytes = max_capacity * size_of::<u64>();
        let freelist = Arc::new(HashSetFreelist::new(num_threads, Some(max_bytes)));

        let threads: Vec<_> = iter::repeat_with(|| {
            let container = container.clone();
            let freelist = Arc::clone(&freelist);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                freelist.try_push(container);
            })
        })
        .take(num_threads)
        .collect();

        for thread in threads {
            thread.join().unwrap();
        }

        let stats = freelist.stats();
        assert_eq!(stats.num_containers, 1);
        assert_eq!(stats.capacity_elems, max_capacity);
        assert_eq!(stats.capacity_bytes, max_bytes);
    }
}
