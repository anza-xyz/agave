//! Block execution conformance harness.

use {
    super::{
        deserialize_accounts, fee_rate_governor_from_proto, new_accounts_for_tests_single_threaded,
        restore_blockhash_queue,
    },
    crate::{
        bank::{
            Bank, BankFieldsToDeserialize, BankRc,
            bank_hash_details::{
                AccountsDetails, BankHashComponents, BankHashDetails, SlotDetails,
            },
        },
        epoch_stakes::VersionedEpochStakes,
        stake_account,
        stake_history::StakeHistory,
        stakes::{DeserializableDelegationStakes, SerdeStakesToStakeFormat, Stakes},
    },
    agave_feature_set::FeatureSet,
    protosol::protos::{
        self, AcctState, BlockContext as ProtoBlockContext, BlockEffects as ProtoBlockEffects,
        PrevVoteAccount as ProtoPrevVoteAccount,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{accounts_hash::AccountsLtHash, ancestors::Ancestors},
    solana_clock::{BankId, DEFAULT_TICKS_PER_SLOT, Epoch},
    solana_cost_model::cost_model::CostModel,
    solana_epoch_schedule::EpochSchedule,
    solana_feature_gate_interface::{self as feature, Feature},
    solana_fee_calculator::FeeRateGovernor,
    solana_hash::Hash,
    solana_inflation::Inflation,
    solana_lattice_hash::lt_hash::LtHash,
    solana_leader_schedule::{LeaderSchedule, NUM_CONSECUTIVE_LEADER_SLOTS},
    solana_pubkey::Pubkey,
    solana_sdk_ids::sysvar,
    solana_stake_interface::state::Delegation,
    solana_svm::{
        conformance::{
            account_state::account_from_proto, fd_hash::fd_hash,
            feature_set::feature_set_from_proto, setup::sysvar_from_accounts,
            versioned_transaction::versioned_transaction_from_proto,
        },
        transaction_processor::ExecutionRecordingConfig,
    },
    solana_svm_timings::ExecuteTimings,
    solana_vote::vote_account::{VoteAccount, VoteAccounts},
    solana_vote_interface::state::{VoteState1_14_11, VoteStateV3, VoteStateV4, VoteStateVersions},
    std::{
        collections::{HashMap, HashSet},
        path::PathBuf,
    },
};
// Imports used only by the FFI entry point, which is excluded from `test` builds.
#[cfg(not(test))]
use {prost::Message, std::ffi::c_int};

// Firedancer-compatible seed for leader schedule hashing
const LEADER_SCHEDULE_HASH_SEED: u64 = 0xDEADFACE;
const SECONDS_PER_YEAR: f64 = 365.242199 * 24.0 * 60.0 * 60.0;

// This is a little bit hacky because there's no direct Agave API that gets us a populated
// Stakes<Delegation> object from a set of account states. Fine, I'll do it myself...
fn build_latest_stake_delegations(
    account_states: &[(Pubkey, AccountSharedData)],
    epoch: Epoch,
    stake_history: &StakeHistory,
    use_fixed_point_stake_math: bool,
) -> DeserializableDelegationStakes {
    let mut stakes = DeserializableDelegationStakes {
        vote_accounts: VoteAccounts::default(),
        stake_delegations: account_states
            .iter()
            .filter(|(_, account)| account.lamports() > 0)
            .filter_map(|(pubkey, account)| {
                if let Ok(stake_account) =
                    stake_account::StakeAccount::<Delegation>::try_from(account.clone())
                {
                    // Skip zero-stake delegations
                    if stake_account.delegation().stake > 0 {
                        return Some((*pubkey, *stake_account.delegation()));
                    }
                }
                None
            })
            .collect(),
        unused: 0,
        epoch,
        stake_history: stake_history.clone(),
    };

    // Then populate the vote accounts
    account_states
        .iter()
        .filter(|(_, account)| account.lamports() > 0)
        .for_each(|(pubkey, account)| {
            if let Ok(vote_account) = VoteAccount::try_from(account.clone()) {
                // We can pass `new_rate_activation_epoch = 0` because the feature is
                // activated on all clusters.
                stakes.vote_accounts.insert(*pubkey, vote_account, || {
                    stakes
                        .stake_delegations
                        .iter()
                        .filter_map(|(_, delegation)| {
                            if delegation.voter_pubkey == *pubkey {
                                if use_fixed_point_stake_math {
                                    Some(delegation.stake_v2(epoch, stake_history, Some(0)))
                                } else {
                                    #[allow(deprecated)]
                                    Some(delegation.stake(epoch, stake_history, Some(0)))
                                }
                            } else {
                                None
                            }
                        })
                        .sum()
                });
            }
        });

    stakes
}

fn synthesize_vote_account(pva: &ProtoPrevVoteAccount) -> (Pubkey, u64, VoteAccount) {
    let vote_pubkey = Pubkey::new_from_array(pva.address.clone().try_into().unwrap());
    let node_pk = Pubkey::new_from_array(pva.node_pubkey.clone().try_into().unwrap());

    let epoch_credits: Vec<(Epoch, u64, u64)> = pva
        .epoch_credits
        .iter()
        .map(|ec| (ec.epoch, ec.credits, ec.prev_credits))
        .collect();

    let versioned = match pva.version() {
        protos::VoteAccountVersion::V11411 => {
            VoteStateVersions::V1_14_11(Box::new(VoteState1_14_11 {
                node_pubkey: node_pk,
                commission: (pva.commission_bps / 100) as u8,
                epoch_credits,
                ..VoteState1_14_11::default()
            }))
        }
        protos::VoteAccountVersion::V3 => VoteStateVersions::new_v3(VoteStateV3 {
            node_pubkey: node_pk,
            commission: (pva.commission_bps / 100) as u8,
            epoch_credits,
            ..VoteStateV3::default()
        }),
        protos::VoteAccountVersion::V4 => VoteStateVersions::new_v4(VoteStateV4 {
            node_pubkey: node_pk,
            inflation_rewards_commission_bps: pva.commission_bps as u16,
            epoch_credits,
            ..VoteStateV4::default()
        }),
    };

    let serialized = bincode::serialize(&versioned).unwrap();
    let mut account = AccountSharedData::new(1, serialized.len(), &solana_sdk_ids::vote::id());
    account.set_data_from_slice(&serialized);

    let vote_account = VoteAccount::try_from(account).unwrap();
    (vote_pubkey, pva.stake, vote_account)
}

// Build stake delegations for previous epochs. Unlike `build_latest_stake_delegations()`,
// this uses the provided votes cache instead of the latest input account states.
#[allow(deprecated)]
fn build_prev_epoch_stakes(
    vote_accounts: &[ProtoPrevVoteAccount],
) -> Stakes<stake_account::StakeAccount<Delegation>> {
    let stakes = DeserializableDelegationStakes {
        vote_accounts: vote_accounts
            .iter()
            .fold(VoteAccounts::default(), |mut acc, pva| {
                let (pubkey, stake, vote_account) = synthesize_vote_account(pva);
                acc.insert(pubkey, vote_account, || stake);
                acc
            }),
        stake_delegations: Vec::default(),
        unused: 0,
        epoch: Epoch::default(),
        stake_history: StakeHistory::default(),
    };

    Stakes::load_from_deserialized_delegations(stakes.clone(), |pubkey| {
        stakes
            .vote_accounts
            .get(pubkey)
            .map(|vote_account| vote_account.account().clone())
    })
    .unwrap()
}

/// Create account state for each active feature in the given feature set.
fn feature_accounts_from_proto(
    proto_features: &protos::FeatureSet,
    feature_set: &FeatureSet,
) -> Vec<(Pubkey, AccountSharedData)> {
    let indexed_features: HashMap<u64, Pubkey> = feature_set
        .active()
        .keys()
        .map(|pubkey| {
            let bytes = pubkey.to_bytes();
            (u64::from_le_bytes(bytes[..8].try_into().unwrap()), *pubkey)
        })
        .collect();
    let feature = Feature {
        activated_at: Some(0),
    };
    // Ensure all feature accounts are rent-exempt
    const FEATURE_ACCOUNT_LAMPORTS: u64 = 100_000_000;
    proto_features
        .features
        .iter()
        .filter_map(|id| indexed_features.get(id).copied())
        .map(|pubkey| {
            (
                pubkey,
                feature::create_account(&feature, FEATURE_ACCOUNT_LAMPORTS),
            )
        })
        .collect()
}

fn inflation_from_proto(input: &protos::Inflation) -> Inflation {
    let mut inflation = Inflation::default();
    inflation.initial = input.initial;
    inflation.terminal = input.terminal;
    inflation.taper = input.taper;
    inflation.foundation = input.foundation;
    inflation.foundation_term = input.foundation_term;
    inflation
}

fn get_changed_accounts(
    initial_accounts: &[AcctState],
    bank: &Bank,
) -> Vec<(Pubkey, AccountSharedData)> {
    let mut changed_accounts = Vec::new();

    for initial_account in initial_accounts {
        let (pubkey, initial_account_data) = account_from_proto(initial_account.clone());
        let initial_account_data = AccountSharedData::from(initial_account_data);

        if let Some(current_account_data) = bank.get_account(&pubkey) {
            if accounts_differ(&initial_account_data, &current_account_data) {
                changed_accounts.push((pubkey, current_account_data));
            }
        } else if initial_account.lamports > 0 {
            changed_accounts.push((pubkey, AccountSharedData::default()));
        }
    }

    changed_accounts
}

fn accounts_differ(account1: &AccountSharedData, account2: &AccountSharedData) -> bool {
    account1.lamports() != account2.lamports()
        || account1.data() != account2.data()
        || account1.owner() != account2.owner()
        || account1.executable() != account2.executable()
}

fn create_changed_accounts_bank_hash_details(
    bank: &Bank,
    initial_accounts: &[AcctState],
) -> Result<BankHashDetails, String> {
    let slot = bank.slot();
    if !bank.is_frozen() {
        return Err(format!(
            "Bank {slot} must be frozen in order to get bank hash details"
        ));
    }

    let full_slot_details = SlotDetails::new_from_bank(bank, true)?;
    let accounts_lt_hash_checksum = full_slot_details
        .bank_hash_components
        .as_ref()
        .map(|components| components.accounts_lt_hash_checksum.clone())
        .unwrap_or_else(|| "unavailable".to_string());

    let changed_accounts = get_changed_accounts(initial_accounts, bank);

    let slot_details = SlotDetails {
        slot,
        bank_hash: bank.hash().to_string(),
        bank_hash_components: Some(BankHashComponents {
            parent_bank_hash: bank.parent_hash().to_string(),
            signature_count: bank.signature_count(),
            last_blockhash: bank.last_blockhash().to_string(),
            accounts_lt_hash_checksum,
            accounts: AccountsDetails {
                accounts: changed_accounts,
            },
        }),
        transactions: Vec::new(),
    };

    Ok(BankHashDetails::new(vec![slot_details]))
}

/// Single-pass mapping during dedup (rotation-compressed).
/// - Build (Pubkey, rotation_idx) entries from the schedule (sampling every 4 slots).
/// - Sort entries by Pubkey bytes for deterministic order.
/// - Dedup in one pass and write mapped indices directly into sched_mapped[rotation_idx].
/// - Hash unique pubkeys and mapped indices into out[0..8] and out[8..16].
///   Returns the number of unique leaders.
pub fn hash_epoch_leaders(
    leader_schedule: &[Pubkey], // per-slot leaders for the whole epoch
    seed: u64,
    out: &mut [u8; 16],
) -> usize {
    // Build composite entries: one per 4-slot rotation
    #[derive(Clone, Copy)]
    struct Entry {
        pk: Pubkey,
        rot_idx: usize,
    }

    let mut entries: Vec<Entry> = leader_schedule
        .iter()
        .step_by(4) // one representative per rotation
        .enumerate()
        .map(|(rot_idx, pk)| Entry { pk: *pk, rot_idx })
        .collect();

    if entries.is_empty() {
        out.fill(0);
        return 0;
    }

    // Sort by pubkey bytes deterministically
    entries.sort_unstable_by_key(|a| a.pk.to_bytes());

    // Dedup + write mapping in a single pass
    let rotations = entries.len();
    let mut sched_mapped: Vec<u32> = vec![0u32; rotations];

    let mut uniq_cnt = 0usize;
    let mut prev_bytes: Option<[u8; 32]> = None;

    for e in &entries {
        let bytes = e.pk.to_bytes();
        if prev_bytes != Some(bytes) {
            uniq_cnt = uniq_cnt.saturating_add(1);
            prev_bytes = Some(bytes);
        }
        // uniq index is uniq_cnt - 1
        sched_mapped[e.rot_idx] = uniq_cnt.saturating_sub(1) as u32;
    }

    // Build unique_pubkeys for hashing (exact size = uniq_cnt)
    let mut unique_pubkeys: Vec<Pubkey> = Vec::with_capacity(uniq_cnt);
    prev_bytes = None;
    for e in &entries {
        let bytes = e.pk.to_bytes();
        if prev_bytes != Some(bytes) {
            unique_pubkeys.push(e.pk);
            prev_bytes = Some(bytes);
        }
    }

    // Hash unique pubkeys
    let pub_bytes: &[u8] = unsafe {
        core::slice::from_raw_parts(
            unique_pubkeys.as_ptr() as *const u8,
            unique_pubkeys
                .len()
                .saturating_mul(core::mem::size_of::<Pubkey>()),
        )
    };
    let h1 = fd_hash(seed, pub_bytes);
    out[0..8].copy_from_slice(&h1.to_le_bytes());

    // Part 2 (last 64 bits): Hash of the compressed schedule (leader indices)
    // This captures the scheduled order of the leaders throughout the epoch
    let sched_bytes: &[u8] = unsafe {
        core::slice::from_raw_parts(
            sched_mapped.as_ptr() as *const u8,
            sched_mapped
                .len()
                .saturating_mul(core::mem::size_of::<u32>()),
        )
    };
    let h2 = fd_hash(seed, sched_bytes);
    out[8..16].copy_from_slice(&h2.to_le_bytes());

    uniq_cnt
}

fn validate_transaction_message(message: &protos::TransactionMessage) {
    for account_key in &message.account_keys {
        let _: [u8; 32] = account_key.as_slice().try_into().unwrap();
    }
    if !message.recent_blockhash.is_empty() {
        let _: [u8; 32] = message.recent_blockhash.as_slice().try_into().unwrap();
    }
    if !message.is_legacy {
        for lookup in &message.address_table_lookups {
            let _: [u8; 32] = lookup.account_key.as_slice().try_into().unwrap();
        }
    }
}

#[allow(deprecated)]
pub fn execute_block(context: &ProtoBlockContext) -> ProtoBlockEffects {
    let bank_ctx = context.bank.as_ref().unwrap();
    let fd_features = bank_ctx.features.clone().unwrap_or_default();
    let feature_set = feature_set_from_proto(&fd_features);

    let current_slot = bank_ctx.slot;
    let parent_slot = bank_ctx.parent_slot;
    let poh = Hash::new_from_array(bank_ctx.poh.as_slice().try_into().unwrap());

    let acct_states_from_proto = deserialize_accounts(&context.acct_states);

    // Epoch schedule
    let epoch_schedule: EpochSchedule =
        sysvar_from_accounts(&acct_states_from_proto, &sysvar::epoch_schedule::id());
    let stake_history: StakeHistory =
        sysvar_from_accounts(&acct_states_from_proto, &sysvar::stake_history::id());

    let lamports_per_signature = u64::from(bank_ctx.rbh_lamports_per_signature);

    let blockhash_queue = restore_blockhash_queue(&bank_ctx.blockhash_queue);

    // Accounts DB config and initialization
    let accounts = new_accounts_for_tests_single_threaded();

    // Create feature gate accounts for all feature gates that are present in the protobuf
    // feature set.
    //
    // These feature gate accounts will be overridden by any account states that are already
    // present in the protobuf, so that it's obvious what the final account state is.
    let all_acct_state_pubkeys_from_proto: HashSet<_> =
        acct_states_from_proto.iter().map(|(pk, _)| *pk).collect();
    let accounts_to_store: Vec<_> = feature_accounts_from_proto(&fd_features, &feature_set)
        .into_iter()
        .filter(|(pk, _)| !all_acct_state_pubkeys_from_proto.contains(pk))
        .chain(acct_states_from_proto.iter().cloned())
        .collect();

    accounts.store_accounts_seq(
        (parent_slot, &accounts_to_store[..]),
        BankId::default(),
        None,
        &Ancestors::default(),
    );
    accounts.store_accounts_seq(
        (current_slot, &accounts_to_store[..]),
        BankId::default(),
        None,
        &Ancestors::default(),
    );
    accounts.accounts_db.add_root(parent_slot);
    let accounts_data_size_initial = accounts_to_store
        .iter()
        .map(|(_, account)| account.data().len() as u64)
        .sum();

    let current_epoch = epoch_schedule.get_epoch(current_slot);
    let parent_epoch = epoch_schedule.get_epoch(parent_slot);
    let leader_schedule_epoch = epoch_schedule.get_leader_schedule_epoch(parent_slot);
    let use_fixed_point_stake_math = feature_set.snapshot().upgrade_bpf_stake_program_to_v5_1;

    let stakes_t = build_latest_stake_delegations(
        &acct_states_from_proto,
        parent_epoch,
        &stake_history,
        use_fixed_point_stake_math,
    );

    // Convert stakes_t (current epoch delegations + stake_history from sysvar) into
    // Stakes<StakeAccount> for the StakesCache. This mirrors new_from_snapshot which
    // loads the StakesCache from the deserialized snapshot stakes.
    let stakes_for_cache = Stakes::load_from_deserialized_delegations(stakes_t.clone(), |pubkey| {
        accounts_to_store
            .iter()
            .find(|(pk, _)| pk == pubkey)
            .map(|(_, acct)| acct.clone())
    })
    .unwrap();

    let stakes_t_1 = build_prev_epoch_stakes(&bank_ctx.vote_accounts_t_1);
    let stakes_t_2 = build_prev_epoch_stakes(&bank_ctx.vote_accounts_t_2);

    // epoch_stakes keyed by absolute epoch:
    // leader_schedule_epoch ← stakes_t_1,
    // leader_schedule_epoch - 1 ← stakes_t_2.
    let mut epoch_stakes: HashMap<Epoch, VersionedEpochStakes> = HashMap::new();
    epoch_stakes.insert(
        leader_schedule_epoch.saturating_sub(1),
        VersionedEpochStakes::new(
            SerdeStakesToStakeFormat::from(stakes_t_2),
            leader_schedule_epoch.saturating_sub(1),
        ),
    );
    epoch_stakes.insert(
        leader_schedule_epoch,
        VersionedEpochStakes::new(
            SerdeStakesToStakeFormat::from(stakes_t_1),
            leader_schedule_epoch,
        ),
    );

    // Source stakes from epoch_stakes[current_epoch] (= bank.epoch_vote_accounts(current_epoch))
    // so the boundary-vs-mid-epoch routing lives only in the map setup above.
    let l_sched = LeaderSchedule::new(
        epoch_stakes
            .get(&current_epoch)
            .expect("epoch_stakes missing current_epoch entry")
            .stakes()
            .vote_accounts()
            .as_ref(),
        current_epoch,
        epoch_schedule.get_slots_in_epoch(current_epoch) as usize,
        NUM_CONSECUTIVE_LEADER_SLOTS,
    );
    let (_, slot_index) = epoch_schedule.get_epoch_and_slot_index(current_slot);
    let leader = l_sched[slot_index];

    let input_fee_rate_governor = bank_ctx.fee_rate_governor.as_ref().unwrap();
    let fee_rate_governor = FeeRateGovernor::new_derived(
        &fee_rate_governor_from_proto(input_fee_rate_governor, lamports_per_signature),
        bank_ctx.parent_signature_count,
    );

    let mut parent_lthash = LtHash::identity();
    for (i, chunk) in bank_ctx.parent_lt_hash.chunks_exact(2).enumerate() {
        parent_lthash.0[i] = u16::from_le_bytes(chunk.try_into().unwrap());
    }

    assert!(bank_ctx.ns_per_slot.len() == 16);
    let ns_per_slot = u128::from_le_bytes(bank_ctx.ns_per_slot[..16].try_into().unwrap());
    let slots_per_year = SECONDS_PER_YEAR * 1e9 / ns_per_slot as f64;
    // Clone epoch_schedule for later use since it will be moved into bank_fields
    let epoch_schedule_for_effects = epoch_schedule.clone();

    let bank_fields = BankFieldsToDeserialize {
        blockhash_queue,
        parent_hash: Hash::new_from_array(bank_ctx.parent_bank_hash.as_slice().try_into().unwrap()),
        parent_slot,
        capitalization: bank_ctx.capitalization,
        tick_height: DEFAULT_TICKS_PER_SLOT.saturating_mul(current_slot),
        max_tick_height: DEFAULT_TICKS_PER_SLOT.saturating_mul(current_slot.saturating_add(1)),
        ticks_per_slot: DEFAULT_TICKS_PER_SLOT,
        ns_per_slot,
        slots_per_year,
        slot: current_slot,
        block_height: bank_ctx.block_height,
        leader_id: leader.id,
        fee_rate_governor,
        epoch_schedule,
        inflation: inflation_from_proto(bank_ctx.inflation.as_ref().unwrap()),
        stakes: stakes_t,
        accounts_lt_hash: AccountsLtHash(parent_lthash),
        ..BankFieldsToDeserialize::default()
    };

    let bank_rc = BankRc::new(accounts);
    let bank = Bank::new_for_block_tests(
        bank_rc,
        bank_fields,
        feature_set,
        epoch_stakes,
        stakes_for_cache,
        accounts_data_size_initial,
    );

    // The bank must be wrapped in `BankForks` so the program cache has a fork graph.
    // Keep `bank_forks` alive for the duration of execution.
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();

    // Sequentially load+execute+commit each txn against the bank.  Bypasses
    // the block-replay scheduler so cross-txn lock conflicts can't gate one
    // txn on another's outcome.
    // Each batch contains one transaction; a future context format can group
    // transactions to exercise intra-batch write-lock conflicts.
    let mut has_err = false;
    for proto_txn in &context.txns {
        let Some(message) = proto_txn.message.as_ref() else {
            has_err = true;
            continue;
        };
        validate_transaction_message(message);
        let versioned_tx = versioned_transaction_from_proto(proto_txn);

        let Ok(batch) = bank.prepare_entry_batch(vec![versioned_tx]) else {
            has_err = true;
            continue;
        };

        let (commit_results, _) = bank.load_execute_and_commit_transactions(
            &batch,
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        );

        for (commit_result, sanitized) in commit_results.iter().zip(batch.sanitized_transactions())
        {
            let Ok(committed) = commit_result else {
                has_err = true;
                continue;
            };
            let tx_cost = CostModel::calculate_cost_for_executed_transaction(
                sanitized,
                committed.executed_units,
                committed.loaded_account_stats.loaded_accounts_data_size,
                &bank.feature_set,
            );
            if bank
                .write_cost_tracker()
                .unwrap()
                .try_add(&tx_cost)
                .is_err()
            {
                has_err = true;
            }
        }
    }

    // Mirror register_recent_blockhash: queue the slot blockhash and refresh the RecentBlockhashes sysvar.
    bank.register_recent_blockhash_for_test(&poh, None);
    bank.update_recent_blockhashes();

    bank.freeze();
    let cost_tracker = bank.read_cost_tracker().unwrap();

    // Emit changed-account bank-hash details for differential-fuzz mismatch analysis.
    // The debug tooling compares this JSON with Firedancer's SOLCAP output.
    if std::env::var("AGAVE_SOLCAP_DIR").is_ok() {
        let details = create_changed_accounts_bank_hash_details(
            bank_forks.read().unwrap().working_bank().as_ref(),
            &context.acct_states,
        )
        .unwrap();

        let parent_dir: PathBuf = std::env::var("AGAVE_SOLCAP_DIR").unwrap().into();
        let path = parent_dir.join(details.filename().unwrap());
        _ = std::fs::create_dir_all(parent_dir);
        let file = std::fs::File::create(&path).unwrap();
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer_pretty(writer, &details).unwrap();
    }

    // Build leader_schedule_effects for consensus verification:

    // The leader schedule determines which validator is allowed to produce blocks
    // for each slot in an epoch. This section computes metadata and a hash of the
    // schedule to enable cross-implementation verification (e.g., Agave vs Firedancer).

    // Calculate the epoch boundaries:
    // - leader_schedule_epoch: The epoch for which the leader schedule applies
    // - first_slot: The absolute slot number where this epoch begins
    // - slots_in_epoch: Total number of slots in this epoch (can vary by epoch)
    let first_slot = epoch_schedule_for_effects.get_first_slot_in_epoch(current_epoch);
    let slots_in_epoch = epoch_schedule_for_effects.get_slots_in_epoch(current_epoch);

    // Generate a deterministic 128-bit hash of the entire leader schedule.
    // This hash encodes both WHO the leaders are and WHEN they lead.
    // We use a fixed seed for reproducibility across implementations.
    let mut schedule_hash = [0u8; 16];
    let schedule_pubkeys: Vec<Pubkey> = (0..slots_in_epoch)
        .map(|slot_offset| l_sched[slot_offset].id)
        .collect();

    let unique_cnt = hash_epoch_leaders(
        &schedule_pubkeys,
        LEADER_SCHEDULE_HASH_SEED,
        &mut schedule_hash,
    );

    // Package all the schedule metadata for output
    let leader_schedule_effects = protos::LeaderScheduleEffects {
        leaders_epoch: current_epoch,     // Which epoch this schedule applies to
        leaders_slot0: first_slot,        // First absolute slot in this epoch
        leaders_slot_cnt: slots_in_epoch, // Total slots in this epoch
        leader_pub_cnt: unique_cnt as u64, // Number of unique leader validators
        leaders_sched_cnt: slots_in_epoch, // Number of scheduled leader slots (verification field)
        leader_schedule_hash: schedule_hash.to_vec(), // 128-bit fingerprint of the schedule
    };

    let bank_hash = if has_err {
        Hash::default()
    } else {
        bank.hash()
    };

    let capitalization = if has_err { 0 } else { bank.capitalization() };

    // Then include in the output
    ProtoBlockEffects {
        has_error: has_err,
        slot_capitalization: capitalization,
        bank_hash: bank_hash.to_bytes().to_vec(),
        cost_tracker: Some(protos::CostTracker {
            block_cost: cost_tracker.block_cost(),
        }),
        leader_schedule: Some(leader_schedule_effects),
    }
}

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point to a
/// writable buffer of at least `*out_psz` bytes. On return, `*out_psz` is
/// updated to the number of bytes written.
//
// Excluded from `test` builds: the symbol would otherwise be defined both here
// and in the `path = "."` dev-dependency rlib, producing a duplicate-symbol link
// error. Tests call the safe `execute_block` API directly.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_block_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    if in_ptr.is_null() || in_sz == 0 {
        return 0;
    }
    if out_psz.is_null() || out_ptr.is_null() {
        return 0;
    }
    let in_slice = unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) };
    let Ok(block_context) = ProtoBlockContext::decode(in_slice) else {
        return 0;
    };

    let block_result = execute_block(&block_context);

    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize) };
    let out_vec = block_result.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }

    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe { *out_psz = out_vec.len() as u64 };

    1
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]

    use {
        super::{LEADER_SCHEDULE_HASH_SEED, execute_block, hash_epoch_leaders},
        protosol::protos::{
            AcctState, BlockBank as ProtoBlockBank, BlockContext as ProtoBlockContext,
            BlockhashQueueEntry as ProtoBlockhashQueueEntry,
            CompiledInstruction as ProtoCompiledInstruction, EpochCredit as ProtoEpochCredit,
            FeatureSet as ProtoFeatureSet, FeeRateGovernor as ProtoFeeRateGovernor,
            Inflation as ProtoInflation,
            MessageAddressTableLookup as ProtoMessageAddressTableLookup,
            MessageHeader as ProtoMessageHeader, PrevVoteAccount as ProtoPrevVoteAccount,
            SanitizedTransaction as ProtoSanitizedTransaction,
            TransactionMessage as ProtoTransactionMessage, VoteAccountVersion,
        },
        solana_account::{Account, create_account_for_test},
        solana_clock::Clock,
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_sdk_ids::{native_loader, system_program, sysvar},
        solana_slot_hashes::SlotHashes,
        solana_slot_history::SlotHistory,
        solana_stake_history::StakeHistory,
        solana_svm::conformance::account_state::account_to_proto,
        solana_sysvar::{
            SysvarSerialize, epoch_rewards::EpochRewards, last_restart_slot::LastRestartSlot,
            recent_blockhashes::RecentBlockhashes,
        },
    };

    const PAYER: [u8; 32] = [0x11; 32];
    const RECIPIENT: [u8; 32] = [0x22; 32];
    const VOTE_ACCOUNT_T_1: [u8; 32] = [0x33; 32];
    const LEADER_NODE_T_1: [u8; 32] = [0x44; 32];
    const RECENT_BLOCKHASH: [u8; 32] = [0x55; 32];
    const POH: [u8; 32] = [0x66; 32];
    const PARENT_BANK_HASH: [u8; 32] = [0x77; 32];
    const VOTE_ACCOUNT_T_2: [u8; 32] = [0x99; 32];
    const LEADER_NODE_T_2: [u8; 32] = [0xaa; 32];
    const LAMPORTS_PER_SIGNATURE: u32 = 5_000;

    fn account(pubkey: Pubkey, lamports: u64, owner: Pubkey) -> AcctState {
        account_to_proto((
            pubkey,
            Account {
                lamports,
                data: Vec::new(),
                owner,
                executable: false,
                rent_epoch: u64::MAX,
            },
        ))
    }

    fn sysvar_account<T: SysvarSerialize>(pubkey: Pubkey, value: &T) -> AcctState {
        account_to_proto((pubkey, create_account_for_test(value)))
    }

    fn block_sysvar_accounts(parent_slot: u64, epoch_schedule: &EpochSchedule) -> Vec<AcctState> {
        let parent_epoch = epoch_schedule.get_epoch(parent_slot);
        let clock = Clock {
            slot: parent_slot,
            epoch_start_timestamp: 1_700_000_000,
            epoch: parent_epoch,
            leader_schedule_epoch: epoch_schedule.get_leader_schedule_epoch(parent_slot),
            unix_timestamp: 1_700_000_000 + parent_slot as i64,
        };
        let mut slot_hashes = SlotHashes::default();
        if parent_slot > 0 {
            slot_hashes.add(parent_slot - 1, Hash::new_from_array(PARENT_BANK_HASH));
        }
        let mut slot_history = SlotHistory::default();
        slot_history.add(parent_slot);

        vec![
            sysvar_account(sysvar::clock::id(), &clock),
            sysvar_account(sysvar::epoch_schedule::id(), epoch_schedule),
            sysvar_account(sysvar::epoch_rewards::id(), &EpochRewards::default()),
            sysvar_account(sysvar::rent::id(), &Rent::default()),
            sysvar_account(sysvar::slot_hashes::id(), &slot_hashes),
            sysvar_account(
                sysvar::recent_blockhashes::id(),
                &RecentBlockhashes::default(),
            ),
            account_to_proto((
                sysvar::stake_history::id(),
                Account {
                    lamports: 1,
                    data: bincode::serialize(&StakeHistory::default()).unwrap(),
                    owner: sysvar::id(),
                    executable: false,
                    rent_epoch: u64::MAX,
                },
            )),
            sysvar_account(sysvar::last_restart_slot::id(), &LastRestartSlot::default()),
            sysvar_account(sysvar::slot_history::id(), &slot_history),
        ]
    }

    fn system_program_account() -> AcctState {
        account_to_proto((
            system_program::id(),
            Account {
                lamports: 1,
                data: b"system_program".to_vec(),
                owner: native_loader::id(),
                executable: true,
                rent_epoch: u64::MAX,
            },
        ))
    }

    fn previous_vote_account(
        vote_account: [u8; 32],
        leader_node: [u8; 32],
    ) -> ProtoPrevVoteAccount {
        ProtoPrevVoteAccount {
            address: vote_account.to_vec(),
            node_pubkey: leader_node.to_vec(),
            stake: 1_000_000,
            commission_bps: 500,
            epoch_credits: vec![
                ProtoEpochCredit {
                    epoch: 0,
                    credits: 100,
                    prev_credits: 0,
                },
                ProtoEpochCredit {
                    epoch: 1,
                    credits: 200,
                    prev_credits: 100,
                },
            ],
            version: VoteAccountVersion::V3 as i32,
        }
    }

    fn block_bank(slot: u64, parent_slot: u64, capitalization: u64) -> ProtoBlockBank {
        ProtoBlockBank {
            blockhash_queue: vec![ProtoBlockhashQueueEntry {
                blockhash: RECENT_BLOCKHASH.to_vec(),
                lamports_per_signature: u64::from(LAMPORTS_PER_SIGNATURE),
            }],
            rbh_lamports_per_signature: LAMPORTS_PER_SIGNATURE,
            fee_rate_governor: Some(ProtoFeeRateGovernor {
                target_lamports_per_signature: u64::from(LAMPORTS_PER_SIGNATURE),
                target_signatures_per_slot: 20_000,
                min_lamports_per_signature: u64::from(LAMPORTS_PER_SIGNATURE),
                max_lamports_per_signature: u64::from(LAMPORTS_PER_SIGNATURE),
                burn_percent: 0,
            }),
            slot,
            parent_slot,
            capitalization,
            ns_per_slot: 400_000_000u128.to_le_bytes().to_vec(),
            inflation: Some(ProtoInflation::default()),
            block_height: slot,
            poh: POH.to_vec(),
            parent_bank_hash: PARENT_BANK_HASH.to_vec(),
            parent_lt_hash: vec![0x88; 2_048],
            parent_signature_count: 0,
            features: Some(ProtoFeatureSet::default()),
            vote_accounts_t_1: vec![previous_vote_account(VOTE_ACCOUNT_T_1, LEADER_NODE_T_1)],
            vote_accounts_t_2: vec![previous_vote_account(VOTE_ACCOUNT_T_2, LEADER_NODE_T_2)],
        }
    }

    fn block_context(slot: u64, parent_slot: u64) -> ProtoBlockContext {
        let epoch_schedule = EpochSchedule::default();
        let mut acct_states = block_sysvar_accounts(parent_slot, &epoch_schedule);
        acct_states.extend([
            account(
                Pubkey::new_from_array(LEADER_NODE_T_1),
                1_000_000,
                system_program::id(),
            ),
            account(
                Pubkey::new_from_array(LEADER_NODE_T_2),
                1_000_000,
                system_program::id(),
            ),
        ]);
        acct_states.push(system_program_account());
        let capitalization = acct_states.iter().map(|account| account.lamports).sum();

        ProtoBlockContext {
            txns: Vec::new(),
            acct_states,
            bank: Some(block_bank(slot, parent_slot, capitalization)),
        }
    }

    fn transfer_transaction(lamports: u64) -> ProtoSanitizedTransaction {
        let mut instruction_data = 2u32.to_le_bytes().to_vec();
        instruction_data.extend_from_slice(&lamports.to_le_bytes());

        ProtoSanitizedTransaction {
            message: Some(ProtoTransactionMessage {
                is_legacy: true,
                header: Some(ProtoMessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                }),
                account_keys: vec![
                    PAYER.to_vec(),
                    RECIPIENT.to_vec(),
                    system_program::id().to_bytes().to_vec(),
                ],
                recent_blockhash: RECENT_BLOCKHASH.to_vec(),
                instructions: vec![ProtoCompiledInstruction {
                    program_id_index: 2,
                    accounts: vec![0, 1],
                    data: instruction_data,
                }],
                address_table_lookups: Vec::new(),
            }),
            message_hash: Vec::new(),
            signatures: vec![vec![0x99; 64]],
        }
    }

    fn transfer_context(lamports: u64) -> ProtoBlockContext {
        let mut context = block_context(1, 0);
        context.acct_states.extend([
            account(
                Pubkey::new_from_array(PAYER),
                10_000_000,
                system_program::id(),
            ),
            account(
                Pubkey::new_from_array(RECIPIENT),
                1_000_000,
                system_program::id(),
            ),
        ]);
        context.bank.as_mut().unwrap().capitalization = context
            .acct_states
            .iter()
            .map(|account| account.lamports)
            .sum();
        context.txns.push(transfer_transaction(lamports));
        context
    }

    fn block_cost(effects: &protosol::protos::BlockEffects) -> u64 {
        effects
            .cost_tracker
            .as_ref()
            .expect("block effects must include cost tracker")
            .block_cost
    }

    fn assert_nonzero_bytes(bytes: &[u8], expected_len: usize) {
        assert_eq!(bytes.len(), expected_len);
        assert!(bytes.iter().any(|byte| *byte != 0));
    }

    fn single_leader_schedule_hash(leader: [u8; 32], slot_count: usize) -> [u8; 16] {
        let schedule = vec![Pubkey::new_from_array(leader); slot_count];
        let mut hash = [0; 16];
        assert_eq!(
            hash_epoch_leaders(&schedule, LEADER_SCHEDULE_HASH_SEED, &mut hash),
            1
        );
        hash
    }

    #[test]
    fn execute_empty_block_is_deterministic() {
        let context = block_context(1, 0);

        let first = execute_block(&context);
        let second = execute_block(&context);

        assert_eq!(first, second);
        assert!(!first.has_error);
        assert_nonzero_bytes(&first.bank_hash, 32);
        assert!(first.slot_capitalization > 0);
        assert_eq!(block_cost(&first), 0);

        let leader_schedule = first
            .leader_schedule
            .as_ref()
            .expect("block effects must include leader schedule");
        assert_eq!(leader_schedule.leaders_epoch, 0);
        assert_eq!(leader_schedule.leaders_slot0, 0);
        assert_eq!(leader_schedule.leaders_slot_cnt, 32);
        assert_eq!(leader_schedule.leader_pub_cnt, 1);
        assert_eq!(leader_schedule.leaders_sched_cnt, 32);
        assert_nonzero_bytes(&leader_schedule.leader_schedule_hash, 16);
        let expected_hash = single_leader_schedule_hash(LEADER_NODE_T_2, 32);
        let swapped_hash = single_leader_schedule_hash(LEADER_NODE_T_1, 32);
        assert_ne!(expected_hash, swapped_hash);
        assert_eq!(
            leader_schedule.leader_schedule_hash.as_slice(),
            expected_hash.as_slice()
        );
    }

    #[test]
    fn execute_empty_block_at_warmup_epoch_boundary() {
        let effects = execute_block(&block_context(32, 31));

        assert!(!effects.has_error);
        assert_nonzero_bytes(&effects.bank_hash, 32);
        assert!(effects.slot_capitalization > 0);
        assert_eq!(block_cost(&effects), 0);

        let leader_schedule = effects
            .leader_schedule
            .as_ref()
            .expect("block effects must include leader schedule");
        assert_eq!(leader_schedule.leaders_epoch, 1);
        assert_eq!(leader_schedule.leaders_slot0, 32);
        assert_eq!(leader_schedule.leaders_slot_cnt, 64);
        assert_eq!(leader_schedule.leader_pub_cnt, 1);
        assert_eq!(leader_schedule.leaders_sched_cnt, 64);
        assert_nonzero_bytes(&leader_schedule.leader_schedule_hash, 16);
        let expected_hash = single_leader_schedule_hash(LEADER_NODE_T_1, 64);
        let swapped_hash = single_leader_schedule_hash(LEADER_NODE_T_2, 64);
        assert_ne!(expected_hash, swapped_hash);
        assert_eq!(
            leader_schedule.leader_schedule_hash.as_slice(),
            expected_hash.as_slice()
        );
    }

    #[test]
    fn committed_system_transfer_changes_bank_hash_with_amount() {
        let one_lamport = execute_block(&transfer_context(1));
        let two_lamports = execute_block(&transfer_context(2));

        assert!(!one_lamport.has_error);
        assert!(!two_lamports.has_error);
        assert_nonzero_bytes(&one_lamport.bank_hash, 32);
        assert_nonzero_bytes(&two_lamports.bank_hash, 32);

        let one_lamport_cost = block_cost(&one_lamport);
        let two_lamport_cost = block_cost(&two_lamports);
        assert!(one_lamport_cost > 0);
        assert_eq!(one_lamport_cost, two_lamport_cost);
        assert_ne!(one_lamport.bank_hash, two_lamports.bank_hash);
    }

    #[test]
    fn missing_transaction_message_returns_error_effects() {
        let mut context = block_context(1, 0);
        context.txns.push(ProtoSanitizedTransaction::default());

        let effects = execute_block(&context);

        assert!(effects.has_error);
        assert_eq!(effects.slot_capitalization, 0);
        assert_eq!(effects.bank_hash, vec![0; 32]);
        assert_eq!(block_cost(&effects), 0);
        let leader_schedule = effects
            .leader_schedule
            .as_ref()
            .expect("error effects must retain leader schedule");
        assert_nonzero_bytes(&leader_schedule.leader_schedule_hash, 16);
    }

    #[test]
    #[should_panic]
    fn malformed_transaction_message_account_key_panics() {
        let mut context = transfer_context(1);
        context.txns[0].message.as_mut().unwrap().account_keys[0] = vec![0; 31];

        execute_block(&context);
    }

    #[test]
    #[should_panic]
    fn malformed_transaction_message_recent_blockhash_panics() {
        let mut context = transfer_context(1);
        context.txns[0].message.as_mut().unwrap().recent_blockhash = vec![0; 31];

        execute_block(&context);
    }

    #[test]
    #[should_panic]
    fn malformed_transaction_message_lookup_key_panics() {
        let mut context = transfer_context(1);
        let message = context.txns[0].message.as_mut().unwrap();
        message.is_legacy = false;
        message
            .address_table_lookups
            .push(ProtoMessageAddressTableLookup {
                account_key: vec![0; 31],
                writable_indexes: Vec::new(),
                readonly_indexes: Vec::new(),
            });

        execute_block(&context);
    }

    #[test]
    fn legacy_transaction_message_ignores_malformed_lookup_key() {
        let mut context = transfer_context(1);
        context.txns[0]
            .message
            .as_mut()
            .unwrap()
            .address_table_lookups
            .push(ProtoMessageAddressTableLookup {
                account_key: vec![0; 31],
                writable_indexes: Vec::new(),
                readonly_indexes: Vec::new(),
            });

        let effects = execute_block(&context);

        assert!(!effects.has_error);
    }

    #[test]
    fn hash_epoch_leaders_empty() {
        let mut hash = [0xff; 16];

        assert_eq!(hash_epoch_leaders(&[], 0xDEAD_FACE, &mut hash), 0);
        assert_eq!(hash, [0; 16]);
    }

    #[test]
    fn hash_epoch_leaders_repeated_rotations_are_deterministic() {
        let leader = Pubkey::new_from_array([0xa1; 32]);
        let schedule = vec![leader; 20];
        let mut first_hash = [0; 16];
        let mut second_hash = [0xff; 16];

        let first_unique_count =
            hash_epoch_leaders(&schedule, LEADER_SCHEDULE_HASH_SEED, &mut first_hash);
        let second_unique_count =
            hash_epoch_leaders(&schedule, LEADER_SCHEDULE_HASH_SEED, &mut second_hash);

        assert_eq!(first_unique_count, 1);
        assert_eq!(second_unique_count, 1);
        assert_eq!(first_hash, second_hash);
        assert_nonzero_bytes(&first_hash, 16);
    }

    #[test]
    fn hash_epoch_leaders_multiple_rotations_are_order_sensitive_and_deterministic() {
        let leader_a = Pubkey::new_from_array([0xa1; 32]);
        let leader_b = Pubkey::new_from_array([0xb2; 32]);
        let leader_c = Pubkey::new_from_array([0xc3; 32]);
        let rotations = [leader_a, leader_b, leader_a, leader_c, leader_b];
        let schedule: Vec<_> = rotations
            .iter()
            .copied()
            .flat_map(|leader| [leader; 4])
            .collect();
        let reversed_schedule: Vec<_> = rotations
            .iter()
            .rev()
            .copied()
            .flat_map(|leader| [leader; 4])
            .collect();
        let mut first_hash = [0; 16];
        let mut second_hash = [0xff; 16];
        let mut reversed_hash = [0; 16];

        let first_unique_count =
            hash_epoch_leaders(&schedule, LEADER_SCHEDULE_HASH_SEED, &mut first_hash);
        let second_unique_count =
            hash_epoch_leaders(&schedule, LEADER_SCHEDULE_HASH_SEED, &mut second_hash);
        let reversed_unique_count = hash_epoch_leaders(
            &reversed_schedule,
            LEADER_SCHEDULE_HASH_SEED,
            &mut reversed_hash,
        );

        assert_eq!(first_unique_count, 3);
        assert_eq!(second_unique_count, 3);
        assert_eq!(reversed_unique_count, 3);
        assert_eq!(first_hash, second_hash);
        assert_ne!(first_hash, reversed_hash);
        assert_nonzero_bytes(&first_hash, 16);
        assert_nonzero_bytes(&reversed_hash, 16);
    }
}
