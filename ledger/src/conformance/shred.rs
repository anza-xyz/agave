//! Differential shred-parse harness: drives Agave's Blockstore pipeline
//! (pre-filter -> parse -> FEC/recovery -> deshred -> tick verify) and emits
//! ShredParseEffects to diff against Firedancer. Signature and PoH checks are
//! not run; the fuzzer re-proofs each FEC set, so Merkle roots derive normally
//! from the shred bytes.

use {
    crate::{
        blockstore::{
            Blockstore, BlockstoreInsertionMetrics, PossibleDuplicateShred,
            blockstore_purge::PurgeType, check_duplicate_shred,
        },
        blockstore_meta::BlockLocation,
        blockstore_processor::verify_ticks,
        shred::{
            CODING_SHREDS_PER_FEC_BLOCK, DATA_SHREDS_PER_FEC_BLOCK, Payload, ReedSolomonCache,
            Shred,
            filter::{ShredFilterContext, ShredRecoveryContext},
            wire,
        },
    },
    agave_feature_set::{FeatureSet, discard_unexpected_data_complete_shreds},
    agave_votor_messages::migration::MigrationStatus,
    protosol::protos::{BlockParseResult, FecSetParseResult, ShredParseContext, ShredParseEffects},
    solana_account::AccountSharedData,
    solana_accounts_db::{
        account_locks::validate_account_locks,
        accounts::Accounts,
        accounts_db::{AccountsDb, AccountsDbConfig},
        accounts_hash::AccountsLtHash,
        accounts_index::{AccountsIndexConfig, IndexLimit},
        ancestors::Ancestors,
        blockhash_queue::BlockhashQueue,
    },
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_fee_calculator::FeeRateGovernor,
    solana_hard_forks::HardForks,
    solana_hash::Hash,
    solana_inflation::Inflation,
    solana_lattice_hash::lt_hash::LtHash,
    solana_message::AccountKeys,
    solana_packet::PACKET_DATA_SIZE,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime::{
        bank::{Bank, BankFieldsToDeserialize, BankHashStats, BankRc},
        epoch_stakes::VersionedEpochStakes,
        stake_history::StakeHistory,
        stakes::{DeserializableStakes, SerdeStakesToStakeFormat, Stakes},
    },
    solana_sdk_ids::sysvar,
    solana_stake_interface::state::Stake,
    solana_streamer::evicting_sender::EvictingSender,
    solana_transaction::sanitized::MAX_TX_ACCOUNT_LOCKS,
    solana_vote::vote_account::VoteAccounts,
    std::{
        borrow::Cow,
        cell::{Cell, RefCell},
        collections::{BTreeMap, HashMap},
        num::NonZeroUsize,
        path::{Path, PathBuf},
        sync::{Arc, atomic::AtomicBool},
    },
};
#[cfg(not(test))]
use {prost_011::Message, std::ffi::c_int};

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point
/// to writable memory of at least `*out_psz` bytes, which need not be
/// initialized. `out_psz` must be a valid, aligned pointer to a `u64`.
/// On success, `*out_psz` is updated to the number of initialized output bytes.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_shred_parse_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *const u8,
    in_sz: u64,
) -> c_int {
    if out_ptr.is_null() || out_psz.is_null() || in_ptr.is_null() || in_sz == 0 {
        return 0;
    }
    let Ok(in_len) = usize::try_from(in_sz) else {
        return 0;
    };
    let in_slice = unsafe { std::slice::from_raw_parts(in_ptr, in_len) };
    let Ok(context) = ShredParseContext::decode(in_slice) else {
        return 0;
    };

    let effects = execute_shred_parse(&context);
    let out_vec = effects.encode_to_vec();
    let Ok(out_cap) = usize::try_from(unsafe { *out_psz }) else {
        return 0;
    };
    if out_vec.len() > out_cap {
        return 0;
    }
    let Ok(out_len) = u64::try_from(out_vec.len()) else {
        return 0;
    };
    unsafe {
        std::ptr::copy_nonoverlapping(out_vec.as_ptr(), out_ptr, out_vec.len());
        *out_psz = out_len;
    }
    1
}

/// Bank ticks-per-slot, so verify_ticks' tick-count check matches mainnet.
const TICKS_PER_SLOT: u64 = 64;
const HASHES_PER_TICK: u64 = 62_500;

thread_local! {
    // Reused across inputs; opening RocksDB per input was ~20% of harness runtime.
    static SHRED_BLOCKSTORE: RefCell<Option<(Blockstore, LedgerGuard)>> = const { RefCell::new(None) };
    // Counts inputs handled on this thread, used to decide when to reopen the blockstore.
    static SHRED_BLOCKSTORE_INPUT_COUNT: Cell<u64> = const { Cell::new(0) };
}

// Purging only marks rows deleted; RocksDB keeps the delete markers around
// and reads slow down as they pile up. So every N inputs we throw the DB
// away and open a fresh one. Measured: 128 is fastest; reopening every
// input is 8x slower, never reopening is 2.6x slower.
const BLOCKSTORE_REOPEN_EVERY: u64 = 128;

// Removes the ledger dir on clean exit; dirs leaked by an aborted process are swept by open_ledger.
struct LedgerGuard(PathBuf);

impl Drop for LedgerGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Open a reused blockstore in a PID-named dir, first sweeping dirs left by dead (aborted) processes.
fn open_ledger() -> (Blockstore, LedgerGuard) {
    let root = std::env::temp_dir().join("solfuzz_shred_bs");
    let _ = std::fs::create_dir_all(&root);
    let proc_root = Path::new("/proc");
    if proc_root.is_dir()
        && let Ok(entries) = std::fs::read_dir(&root)
    {
        for entry in entries.flatten() {
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.split('-').next()?.parse::<u32>().ok())
                && !proc_root.join(pid.to_string()).exists()
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let dir = root.join(format!(
        "{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    if let Err(err) = std::fs::remove_dir_all(&dir)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        panic!("remove stale ledger dir {}: {err}", dir.display());
    }
    std::fs::create_dir_all(&dir).expect("create ledger dir");
    let blockstore = Blockstore::open(&dir).expect("blockstore open");
    (blockstore, LedgerGuard(dir))
}

pub fn execute_shred_parse(ctx: &ShredParseContext) -> ShredParseEffects {
    let shred_version = ctx.shred_version as u16;
    let root_slot: Slot = ctx.root_slot;
    let discard = ctx
        .features
        .as_ref()
        .map(|features| features.discard_unexpected_data_complete_shreds)
        .unwrap_or(false);

    let mut effects = ShredParseEffects {
        block_parse_result: BlockParseResult::Accepted as i32,
        shred_results: Vec::with_capacity(ctx.shreds.len()),
        fec_set_results: Vec::new(),
    };

    // Bank at the root slot: drives the filter/recovery slot bounds, verify_ticks
    // tick params, and the dup feature gate.
    // FD: resolver + sched config (shred_version, advance_slot_old(root_slot), discard feature).
    let mut feature_set = FeatureSet::default();
    if discard {
        feature_set.activate(&discard_unexpected_data_complete_shreds::id(), 0);
    }
    let bank = build_root_bank(root_slot, feature_set);
    let migration = MigrationStatus::default();

    // Step 1: parse each shred; record whether it's a chained Merkle shred.
    // FD: fd_shred_parse + fd_shred_is_chained. should_discard_shred only gates accumulation.
    let mut filter = ShredFilterContext::new(bank.clone(), shred_version);
    let mut parsed: Vec<Shred> = Vec::with_capacity(ctx.shreds.len());
    for entry in &ctx.shreds {
        let payload = entry.as_slice();
        let parsed_shred = Shred::new_from_serialized_shred(entry.clone()).ok();
        let parses_as_chained = parsed_shred
            .as_ref()
            .is_some_and(|shred| shred.chained_merkle_root().is_ok());
        effects.shred_results.push(parses_as_chained);
        if !filter.should_discard_shred(payload)
            && let Some(shred) = parsed_shred
        {
            parsed.push(shred);
        }
    }

    // FEC accumulate / recover / dedup in an ephemeral blockstore.
    // FD: fd_fec_resolver_add_shred.
    // Reuse the thread-local blockstore (emptied between inputs), reopening it fresh every BLOCKSTORE_REOPEN_EVERY inputs.
    let inputs_seen = SHRED_BLOCKSTORE_INPUT_COUNT.with(|count| {
        let current = count.get();
        count.set(current.wrapping_add(1));
        current
    });
    let cached = SHRED_BLOCKSTORE.with(|cache| cache.borrow_mut().take());
    let (blockstore, ledger) = match cached {
        Some((blockstore, ledger)) if !inputs_seen.is_multiple_of(BLOCKSTORE_REOPEN_EVERY) => {
            blockstore
                .purge_slots(0, u64::MAX, PurgeType::Exact)
                .expect("blockstore reset");
            (blockstore, ledger)
        }
        stale => {
            // Dropping closes the old DB and deletes its directory, which must finish before reopening the same path.
            if let Some((blockstore, ledger)) = stale {
                drop(blockstore);
                drop(ledger);
            }
            open_ledger()
        }
    };
    let (retransmit_sender, _retransmit_rx) = EvictingSender::<Vec<Payload>>::new_bounded(0);
    let mut recovery = ShredRecoveryContext::new(
        ReedSolomonCache::default(),
        retransmit_sender,
        bank.clone(),
        shred_version,
    );
    let mut metrics = BlockstoreInsertionMetrics::default();
    let handle_duplicate = |duplicate: PossibleDuplicateShred| {
        let _ = check_duplicate_shred(
            &blockstore,
            duplicate,
            false, // no_verify_chained_merkle_root: keep pre-Alpenglow validation
        );
    };
    let shreds_iter = parsed.iter().map(|shred| {
        (
            Cow::Borrowed(shred),
            /*is_repaired:*/ false,
            BlockLocation::Original,
        )
    });
    let _ = blockstore.insert_shreds_at_location_handle_duplicate(
        shreds_iter,
        false, // is_trusted: keep dedup + integrity checks
        &mut recovery,
        &handle_duplicate,
        &mut metrics,
    );

    let mut slots: Vec<Slot> = parsed.iter().map(Shred::slot).collect();
    slots.sort_unstable();
    slots.dedup();

    // Deshred + tick verify per slot (PoH intentionally not run).
    // FD: fd_sched_fec_ingest (PoH verify bypassed).
    for &slot in &slots {
        let Ok((entries, _num_shreds, is_full)) =
            blockstore.get_slot_entries_with_shred_info(slot, 0, false)
        else {
            effects.block_parse_result = BlockParseResult::RejectedInvalidHeader as i32;
            continue;
        };
        if entries.is_empty() {
            continue;
        }
        // Mirror FD's fd_sched_parse_txn: reject non-sanitizable, duplicate-account, or over-MTU txns.
        for tx in entries.iter().flat_map(|entry| &entry.transactions) {
            let oversized =
                bincode::serialized_size(tx).map_or(true, |size| size > PACKET_DATA_SIZE as u64);
            let bad_locks = validate_account_locks(
                AccountKeys::new(tx.message.static_account_keys(), None),
                MAX_TX_ACCOUNT_LOCKS,
            )
            .is_err();
            if tx.sanitize().is_err() || bad_locks || oversized {
                effects.block_parse_result = BlockParseResult::RejectedInvalidHeader as i32;
            }
        }
        let mut tick_hash_count = 0u64;
        if verify_ticks(&bank, &entries, is_full, &mut tick_hash_count, &migration).is_err() {
            effects.block_parse_result = BlockParseResult::RejectedInvalidHeader as i32;
        }
    }

    // Emit one result per completed FEC set.
    // FD: capture_completed_fec + reasm pop -> fec_set_results.
    for &slot in &slots {
        let data_shreds = blockstore
            .get_data_shreds_for_slot(slot, 0)
            .unwrap_or_default();
        let mut by_fec: BTreeMap<u32, Vec<Shred>> = BTreeMap::new();
        for shred in data_shreds {
            by_fec.entry(shred.fec_set_index()).or_default().push(shred);
        }

        // Mirror FD reasm: deliver the contiguous chain-validated prefix from index 0, rejecting at the first set that doesn't chain to its predecessor.
        // FD rejects a slot that has a complete FEC set but no complete index-0 set, since the slot's first set must chain to the parent.
        let has_complete_set0 = by_fec
            .get(&0)
            .is_some_and(|group| group.len() == DATA_SHREDS_PER_FEC_BLOCK);
        if !has_complete_set0
            && by_fec
                .values()
                .any(|group| group.len() == DATA_SHREDS_PER_FEC_BLOCK)
        {
            effects.block_parse_result = BlockParseResult::RejectedInvalidHeader as i32;
            continue;
        }

        let mut expected_index = 0u32;
        let mut prev_merkle_root: Option<Vec<u8>> = None;
        for (fec_set_index, mut group) in by_fec {
            if group.len() != DATA_SHREDS_PER_FEC_BLOCK || fec_set_index != expected_index {
                break;
            }
            group.sort_unstable_by_key(Shred::index);
            let first = &group[0];
            let parent = first.parent().unwrap_or(slot);
            // FD: fd_shred_merkle_root(base_data), derived from the proof bytes.
            let (Ok(merkle_root), Ok(chained_merkle_root)) =
                (first.merkle_root(), first.chained_merkle_root())
            else {
                effects.block_parse_result = BlockParseResult::RejectedInvalidHeader as i32;
                break;
            };
            let merkle_root = merkle_root.to_bytes().to_vec();
            let chained_merkle_root = chained_merkle_root.to_bytes().to_vec();
            if let Some(previous) = &prev_merkle_root
                && chained_merkle_root != *previous
            {
                effects.block_parse_result = BlockParseResult::RejectedInvalidHeader as i32;
                break;
            }
            // Concatenate each data shred's data region (matches FD; not deshred,
            // which requires the set to end on a DATA_COMPLETE boundary).
            let mut payload = Vec::new();
            for shred in &group {
                if let Ok(data) = wire::get_data(shred.payload()) {
                    payload.extend_from_slice(data);
                }
            }

            prev_merkle_root = Some(merkle_root.clone());
            expected_index += DATA_SHREDS_PER_FEC_BLOCK as u32;

            effects.fec_set_results.push(FecSetParseResult {
                completed: true,
                merkle_root,
                chained_merkle_root,
                payload,
                slot,
                fec_set_index,
                parent_offset: slot.saturating_sub(parent) as u32,
                shred_version: ctx.shred_version,
                num_data_shreds: group.len() as u32,
                // FD reconstructs + reports 32 coding; mirror that fixed count.
                num_coding_shreds: CODING_SHREDS_PER_FEC_BLOCK as u32,
            });
        }
    }

    // `purge_slots` uses an exclusive RocksDB range end and cannot clear slot
    // `u64::MAX`, so do not cache a blockstore that has seen that slot.
    if slots.last().copied() == Some(u64::MAX) {
        drop(blockstore);
        drop(ledger);
    } else {
        // Return the blockstore to the thread-local for reuse on the next input.
        SHRED_BLOCKSTORE.with(|cache| *cache.borrow_mut() = Some((blockstore, ledger)));
    }

    effects
}

/// Minimal bank at `root_slot` with the caller-provided `feature_set`: supplies
/// the filter/recovery slot bounds, verify_ticks tick params (one slot of ticks,
/// PoH off), and the dup feature gate. Only the rent sysvar is stored, which
/// new_for_txn_tests requires.
fn build_root_bank(root_slot: Slot, feature_set: FeatureSet) -> Arc<Bank> {
    let epoch_schedule = EpochSchedule::default();
    let epoch = epoch_schedule.get_epoch(root_slot);
    let parent_slot = root_slot.saturating_sub(1);

    let accounts = create_accounts_db(vec![]);
    let rent_account = AccountSharedData::new_data(1, &Rent::default(), &sysvar::id()).unwrap();
    accounts.store_accounts_seq(
        (parent_slot, &[(sysvar::rent::id(), rent_account)][..]),
        0,
        None,
        &Ancestors::default(),
    );
    accounts.accounts_db.add_root(parent_slot);
    let bank_rc = BankRc::new(accounts);

    let stakes = DeserializableStakes {
        vote_accounts: VoteAccounts::default(),
        stake_delegations: vec![],
        unused: 0,
        epoch,
        stake_history: StakeHistory::default(),
    };
    let mut epoch_stakes: HashMap<Epoch, VersionedEpochStakes> = HashMap::new();
    for key in [epoch, epoch.saturating_add(1)] {
        epoch_stakes.insert(
            key,
            VersionedEpochStakes::new(
                SerdeStakesToStakeFormat::Stake(Stakes::<Stake>::default()),
                key,
            ),
        );
    }

    let fields = BankFieldsToDeserialize {
        blockhash_queue: BlockhashQueue::default(),
        hash: Hash::default(),
        parent_hash: Hash::default(),
        parent_slot,
        hard_forks: HardForks::default(),
        transaction_count: 0,
        tick_height: TICKS_PER_SLOT.saturating_mul(root_slot),
        signature_count: 0,
        capitalization: 0,
        max_tick_height: TICKS_PER_SLOT.saturating_mul(root_slot.saturating_add(1)),
        hashes_per_tick: Some(HASHES_PER_TICK),
        ticks_per_slot: TICKS_PER_SLOT,
        ns_per_slot: 0,
        genesis_creation_time: 0,
        slots_per_year: 0.0,
        slot: root_slot,
        block_height: root_slot,
        leader_id: Pubkey::default(),
        fee_rate_governor: FeeRateGovernor::default(),
        epoch_schedule,
        inflation: Inflation::default(),
        stakes,
        versioned_epoch_stakes: vec![],
        is_delta: false,
        accounts_data_len: 0,
        accounts_lt_hash: AccountsLtHash(LtHash::identity()),
        bank_hash_stats: BankHashStats::default(),
        block_id: None,
    };

    Arc::new(Bank::new_for_txn_tests(
        bank_rc,
        fields,
        feature_set,
        epoch_stakes,
    ))
}

fn create_accounts_db(paths: Vec<PathBuf>) -> Accounts {
    let index = Some(AccountsIndexConfig {
        bins: Some(2),
        num_flush_threads: Some(NonZeroUsize::new(1).unwrap()),
        index_limit: IndexLimit::InMemOnly,
        ..AccountsIndexConfig::default()
    });
    let accounts_db_config = AccountsDbConfig {
        index,
        skip_initial_hash_calc: true,
        num_background_threads: Some(NonZeroUsize::new(1).unwrap()),
        num_foreground_threads: Some(NonZeroUsize::new(1).unwrap()),
        exhaustively_verify_refcounts: false,
        read_cache_num_shards: Some(2),
        ..AccountsDbConfig::default()
    };
    let accounts_db = AccountsDb::new_with_config(
        paths,
        accounts_db_config,
        None,
        Arc::new(AtomicBool::new(false)),
    );
    Accounts::new(Arc::new(accounts_db))
}
