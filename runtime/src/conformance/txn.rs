//! Transaction conformance harness.
//!
//! Split into two layers, mirroring the SVM harness convention:
//!
//! * The **native** core ([`execute_txn`] + [`BankTxnProcessingResult`]) builds a
//!   [`Bank`] via [`Bank::new_for_txn_tests`], runs
//!   `bank.load_and_execute_transactions`, and returns the native execution
//!   result. It depends only on `solana-runtime`/SVM types, so it is available
//!   under `dev-context-only-utils` and is what the unit tests exercise.
//! * The **conformance** layer (gated by the `conformance` feature) is the
//!   protobuf glue: it decodes a `TxnContext`, converts it into native inputs,
//!   calls [`execute_txn`], encodes the effects as a `TxnResult`, and exposes the
//!   `sol_compat_txn_execute_v1` FFI entry point.
//!
//! Living inside `solana-runtime` lets the harness use the real `Bank` execution
//! path (rather than driving the SVM directly), which keeps it at parity with
//! SolFuzz-Agave.

use {
    super::new_accounts_for_tests_single_threaded,
    crate::{
        bank::{Bank, BankFieldsToDeserialize, BankRc},
        epoch_stakes::VersionedEpochStakes,
        stake_history::StakeHistory,
        stakes::{DeserializableDelegationStakes, SerdeStakesToStakeFormat, Stakes},
    },
    agave_feature_set::FeatureSet,
    agave_transaction_view::transaction_view::UnsanitizedTransactionView,
    bytes::Bytes,
    solana_account::AccountSharedData,
    solana_accounts_db::{ancestors::Ancestors, blockhash_queue::BlockhashQueue},
    solana_clock::{BankId, Clock, DEFAULT_TICKS_PER_SLOT, Epoch, MAX_PROCESSING_AGE},
    solana_epoch_schedule::EpochSchedule,
    solana_fee_calculator::FeeRateGovernor,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::ReplayTransaction,
    solana_sdk_ids::sysvar,
    solana_stake_interface::state::Stake,
    solana_svm::{
        conformance::setup::sysvar_from_accounts,
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_processing_result::TransactionProcessingResult,
        transaction_processor::{ExecutionRecordingConfig, TransactionProcessingConfig},
    },
    solana_svm_timings::ExecuteTimings,
    solana_transaction::{TransactionVerificationMode, versioned::VersionedTransaction},
    solana_transaction_error::TransactionError,
    solana_vote::vote_account::VoteAccounts,
    std::collections::HashMap,
};
#[cfg(feature = "conformance")]
use {
    super::{deserialize_accounts, fee_rate_governor_from_proto, restore_blockhash_queue},
    agave_feature_set::virtual_address_space_adjustments,
    ahash::AHashSet,
    protosol::protos::{TxnContext as ProtoTxnContext, TxnResult as ProtoTxnResult},
    solana_account::Account,
    solana_message::SanitizedMessage,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_signature::Signature,
    solana_svm::conformance::{
        direct_mapping::direct_mapping_handle_cu_exhaustion, feature_set::feature_set_from_proto,
        txn::effects::TxnEffects, versioned_transaction::versioned_transaction_from_proto,
    },
    solana_svm::rollback_accounts::RollbackAccounts,
    solana_svm::transaction_processing_result::ProcessedTransaction,
};
// Imports used only by the FFI entry point, which is excluded from `test` builds.
#[cfg(all(feature = "conformance", not(test)))]
use {prost::Message, std::ffi::c_int};

/// Result of executing a single transaction through the [`Bank`].
pub enum BankTxnProcessingResult {
    /// The transaction failed verification before processing.
    FailedVerification(TransactionError),
    /// The transaction was processed (executed, fees-only, or no-op). Carries the
    /// processing result and transaction for effect extraction.
    Processed {
        result: TransactionProcessingResult,
        runtime_transaction: Box<ReplayTransaction>,
    },
}

/// Build a [`Bank`] from the supplied native inputs and execute `transaction`.
///
/// The clock and epoch-schedule sysvars are read out of `accounts` to derive the
/// bank's slot/epoch.
pub fn execute_txn(
    accounts: &[(Pubkey, AccountSharedData)],
    feature_set: FeatureSet,
    blockhash_queue: BlockhashQueue,
    fee_rate_governor: FeeRateGovernor,
    total_epoch_stake: u64,
    transaction: VersionedTransaction,
) -> BankTxnProcessingResult {
    // Slot and parent slot come from the clock sysvar.
    let clock: Clock = sysvar_from_accounts(accounts, &sysvar::clock::id());
    let slot = clock.slot;
    let parent_slot = slot.saturating_sub(1);

    let epoch_schedule: EpochSchedule =
        sysvar_from_accounts(accounts, &sysvar::epoch_schedule::id());
    let epoch = epoch_schedule.get_epoch(slot);

    // Populate the accounts DB with the input accounts at the parent slot.
    let bank_accounts = new_accounts_for_tests_single_threaded();
    let ancestors = Ancestors::from(vec![parent_slot]);
    bank_accounts.store_accounts((parent_slot, accounts), BankId::default(), None, &ancestors);
    bank_accounts.accounts_db.add_root(parent_slot);
    let bank_rc = BankRc::new(bank_accounts);

    // Dummy epoch stakes with the provided total stake at the current and next epoch.
    let mut epoch_stakes: HashMap<Epoch, VersionedEpochStakes> = HashMap::new();
    for key in [epoch, epoch.saturating_add(1)] {
        let mut entry = VersionedEpochStakes::new(
            SerdeStakesToStakeFormat::Stake(Stakes::<Stake>::default()),
            key,
        );
        entry.set_total_stake(total_epoch_stake);
        epoch_stakes.insert(key, entry);
    }

    // `new_for_txn_tests` ignores `stakes`/`versioned_epoch_stakes`, but the
    // struct still has to be constructed.
    let stakes = DeserializableDelegationStakes {
        vote_accounts: VoteAccounts::default(),
        stake_delegations: vec![],
        unused: 0,
        epoch,
        stake_history: StakeHistory::default(),
    };

    let bank_fields = BankFieldsToDeserialize {
        blockhash_queue,
        parent_slot,
        tick_height: DEFAULT_TICKS_PER_SLOT.saturating_mul(slot),
        max_tick_height: DEFAULT_TICKS_PER_SLOT.saturating_mul(slot.saturating_add(1)),
        ticks_per_slot: DEFAULT_TICKS_PER_SLOT,
        slot,
        block_height: slot,
        fee_rate_governor,
        epoch_schedule,
        stakes,
        ..BankFieldsToDeserialize::default()
    };

    // The bank must be wrapped in `BankForks` so the program cache has a fork graph;
    // `_bank_forks` is kept alive for the duration of execution.
    let bank = Bank::new_for_txn_tests(bank_rc, bank_fields, feature_set, epoch_stakes);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();

    let transaction_bytes = Bytes::from(wincode::serialize(&transaction).unwrap());
    let Ok(transaction_view) = UnsanitizedTransactionView::try_new_unsanitized(transaction_bytes)
    else {
        return BankTxnProcessingResult::FailedVerification(TransactionError::SanitizeFailure);
    };

    let runtime_transaction = match bank.verify_transaction(
        transaction_view,
        TransactionVerificationMode::HashAndVerifyPrecompiles,
    ) {
        Ok(tx) => tx,
        Err(err) => return BankTxnProcessingResult::FailedVerification(err),
    };

    let recording_config = ExecutionRecordingConfig {
        enable_cpi_recording: false,
        enable_log_recording: true,
        enable_return_data_recording: true,
        enable_transaction_balance_recording: false,
    };
    let processing_config = TransactionProcessingConfig {
        recording_config,
        limit_to_load_programs: true,
        ..Default::default()
    };

    let mut timings = ExecuteTimings::default();
    let mut metrics = TransactionErrorMetrics::default();
    let result = {
        let batch = bank.prepare_locked_batch_from_single_tx(&runtime_transaction);
        bank.load_and_execute_transactions(
            &batch,
            MAX_PROCESSING_AGE,
            &mut timings,
            &mut metrics,
            processing_config,
        )
        .processing_results
        .into_iter()
        .next()
        .expect("single transaction execution must return one result")
    };

    BankTxnProcessingResult::Processed {
        result,
        runtime_transaction: Box::new(runtime_transaction),
    }
}

#[cfg(feature = "conformance")]
fn rollback_accounts_to_native(rollback_accounts: &RollbackAccounts) -> Vec<(Pubkey, Account)> {
    rollback_accounts
        .iter()
        .map(|(pubkey, account)| (*pubkey, account.clone().into()))
        .collect()
}

#[cfg(feature = "conformance")]
fn processed_transaction_effects(
    txn: &ProcessedTransaction,
    sanitized_message: &SanitizedMessage,
) -> TxnEffects {
    let (resulting_accounts, rollback_accounts, return_data, compute_unit_limit) = match txn {
        ProcessedTransaction::Executed(executed_tx) => {
            let loaded = &executed_tx.loaded_transaction;
            let resulting_accounts = loaded
                .accounts
                .iter()
                .enumerate()
                .filter(|(index, _)| sanitized_message.is_writable(*index))
                .map(|(_, (pubkey, account))| (*pubkey, account.clone().into()))
                .collect();
            let rollback_accounts = if executed_tx.execution_details.status.is_err() {
                rollback_accounts_to_native(&loaded.rollback_accounts)
            } else {
                vec![]
            };
            let return_data = executed_tx
                .execution_details
                .return_data
                .as_ref()
                .map(|info| info.data.clone())
                .unwrap_or_default();
            (
                resulting_accounts,
                rollback_accounts,
                return_data,
                loaded.compute_budget.compute_unit_limit,
            )
        }
        ProcessedTransaction::FeesOnly(tx) => (
            vec![],
            rollback_accounts_to_native(&tx.rollback_accounts),
            vec![],
            0,
        ),
        ProcessedTransaction::NoOp(_) => (vec![], vec![], vec![], 0),
    };

    let executed_units = txn.executed_units();
    TxnEffects {
        executed: true,
        status: txn.status(),
        resulting_accounts,
        rollback_accounts,
        return_data,
        executed_units,
        fee_details: txn.fee_details(),
        loaded_accounts_data_size: u64::from(txn.loaded_accounts_data_size()),
        // The bank records logs, but the fixture does not compare them.
        logs: vec![],
        cu_avail: compute_unit_limit.saturating_sub(executed_units),
    }
}

#[cfg(feature = "conformance")]
fn unprocessed_txn_result(err: TransactionError) -> ProtoTxnResult {
    ProtoTxnResult {
        fee_details: None,
        ..ProtoTxnResult::from(TxnEffects::from_unprocessed_error(err))
    }
}

/// Decode a `TxnContext` proto, run it through [`execute_txn`], and encode the
/// effects as a `TxnResult` proto.
#[cfg(feature = "conformance")]
pub fn execute_txn_proto(context: &ProtoTxnContext) -> ProtoTxnResult {
    let txn_bank = context.bank.as_ref().unwrap();

    let accounts = deserialize_accounts(&context.account_shared_data);
    let blockhash_queue = restore_blockhash_queue(&txn_bank.blockhash_queue);

    // On snapshot boot the fee rate governor's lamports_per_signature comes from
    // the manifest, so use the provided value directly.
    let input_fee_rate_governor = txn_bank.fee_rate_governor.as_ref().unwrap();
    let fee_rate_governor = fee_rate_governor_from_proto(
        input_fee_rate_governor,
        u64::from(txn_bank.rbh_lamports_per_signature),
    );

    let feature_set = txn_bank
        .features
        .as_ref()
        .map(feature_set_from_proto)
        .unwrap();
    let virtual_address_space_adjustments_active =
        feature_set.is_active(&virtual_address_space_adjustments::id());

    let tx = context.tx.as_ref().unwrap();
    let proto_message = tx.message.as_ref().unwrap();
    let mut transaction = versioned_transaction_from_proto(tx);
    if transaction.signatures.is_empty() {
        // Default: a single empty signature (keeps simple cases valid).
        transaction.signatures.push(Signature::default());
    }

    let (result, runtime_transaction) = match execute_txn(
        &accounts,
        feature_set,
        blockhash_queue,
        fee_rate_governor,
        txn_bank.total_epoch_stake,
        transaction,
    ) {
        BankTxnProcessingResult::FailedVerification(err) => {
            let mut txn_result = unprocessed_txn_result(err);
            // Precompile error codes are not conformant, so they are ignored here.
            txn_result.custom_error = 0;
            return txn_result;
        }
        BankTxnProcessingResult::Processed {
            result,
            runtime_transaction,
        } => (result, runtime_transaction),
    };
    let sanitized_transaction = runtime_transaction.as_sanitized_transaction();
    let sanitized_message = sanitized_transaction.message();

    let mut effects = match &result {
        Ok(txn) => processed_transaction_effects(txn, sanitized_message),
        Err(err) => return unprocessed_txn_result(err.clone()),
    };
    effects.zero_precompile_custom_error(sanitized_message);

    // Only keep modified accounts that were passed in as account keys or were
    // loaded via an address lookup table.
    let mut loaded_account_keys = AHashSet::<Pubkey>::new();
    loaded_account_keys.extend(
        proto_message
            .account_keys
            .iter()
            .map(|key| Pubkey::try_from(key.as_slice()).unwrap()),
    );
    if let SanitizedMessage::V0(message) = sanitized_message {
        loaded_account_keys.extend(message.loaded_addresses.writable.iter().copied());
        loaded_account_keys.extend(message.loaded_addresses.readonly.iter().copied());
    }
    effects
        .resulting_accounts
        .retain(|(pubkey, _)| loaded_account_keys.contains(pubkey));

    let cu_avail = effects.cu_avail;
    let has_err = effects.status.is_err();
    let mut txn_result = ProtoTxnResult::from(effects);

    direct_mapping_handle_cu_exhaustion(
        virtual_address_space_adjustments_active,
        cu_avail,
        has_err,
        txn_result
            .modified_accounts
            .iter_mut()
            .map(|acc| &mut acc.data),
    );

    txn_result
}

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point to a
/// writable buffer of at least `*out_psz` bytes. On return, `*out_psz` is
/// updated to the number of bytes written.
//
// Excluded from `test` builds: the symbol would otherwise be defined both here
// and in the `path = "."` dev-dependency rlib, producing a duplicate-symbol link
// error. Tests call the native `execute_txn` directly.
#[cfg(all(feature = "conformance", not(test)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_txn_execute_v1(
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
    let Ok(context) = ProtoTxnContext::decode(in_slice) else {
        return 0;
    };

    let txn_result = execute_txn_proto(&context);

    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize) };
    let out_vec = txn_result.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe { *out_psz = out_vec.len() as u64 };

    1
}

#[cfg(all(test, feature = "conformance"))]
mod tests {
    use {
        super::{ProtoTxnResult, execute_txn_proto},
        agave_feature_set::{FEATURE_NAMES, disable_sbpf_v0_execution},
        protosol::protos::{
            BlockhashQueueEntry as ProtoBlockhashQueueEntry,
            CompiledInstruction as ProtoCompiledInstruction, FeatureSet as ProtoFeatureSet,
            FeeRateGovernor as ProtoFeeRateGovernor,
            MessageAddressTableLookup as ProtoMessageAddressTableLookup,
            MessageHeader as ProtoMessageHeader, SanitizedTransaction as ProtoSanitizedTransaction,
            TransactionMessage as ProtoTransactionMessage, TxnBank as ProtoTxnBank,
            TxnContext as ProtoTxnContext,
        },
        solana_account::AccountSharedData,
        solana_address_lookup_table_interface::state::{AddressLookupTable, LookupTableMeta},
        solana_clock::Clock,
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_loader_v3_interface::state::UpgradeableLoaderState,
        solana_message::{
            MessageHeader, VersionedMessage,
            compiled_instruction::CompiledInstruction,
            legacy,
            v0::{self, MessageAddressTableLookup},
        },
        solana_pubkey::Pubkey,
        solana_sdk_ids::{bpf_loader_upgradeable, native_loader, sysvar},
        solana_signature::Signature,
        solana_slot_hashes::SlotHashes,
        solana_svm::{
            conformance::account_state::account_to_proto,
            transaction_processing_result::ProcessedTransaction,
        },
        solana_transaction::versioned::VersionedTransaction,
        std::{borrow::Cow, collections::HashSet, env, fs, sync::Arc},
    };

    const LAMPORTS_PER_SIGNATURE: u64 = 5000;

    /// A fixture addresses a feature by the first eight bytes of its pubkey.
    fn feature_id(pubkey: &Pubkey) -> u64 {
        u64::from_le_bytes(pubkey.to_bytes()[..8].try_into().unwrap())
    }

    /// Every feature except `disable_sbpf_v0_execution`, so the v0
    /// `complex-transfer` program loads. `set_exempt_rent_epoch_max` is among
    /// them, matching the accounts' `u64::MAX` rent epoch.
    ///
    /// Filtering happens in fixture-id space, not pubkey space:
    /// `reenable_sbpf_v0_execution` shares its first eight bytes with
    /// `disable_sbpf_v0_execution`, so leaving it in would re-add the same id
    /// and `feature_set_from_proto` would resolve it to either feature.
    fn proto_feature_set() -> ProtoFeatureSet {
        let disabled = feature_id(&disable_sbpf_v0_execution::id());
        ProtoFeatureSet {
            features: FEATURE_NAMES
                .keys()
                .map(feature_id)
                .filter(|id| *id != disabled)
                .collect(),
        }
    }

    /// A blockhash queue with two registered hashes; returns the queue plus the
    /// most-recent blockhash to use as the message's `recent_blockhash`.
    fn proto_blockhash_queue() -> (Vec<ProtoBlockhashQueueEntry>, Hash) {
        let recent = Hash::new_unique();
        let entries = [Hash::new_unique(), recent]
            .iter()
            .map(|blockhash| ProtoBlockhashQueueEntry {
                blockhash: blockhash.to_bytes().to_vec(),
                lamports_per_signature: LAMPORTS_PER_SIGNATURE,
            })
            .collect();
        (entries, recent)
    }

    /// The protobuf form of a transaction, as a fixture would carry it.
    fn proto_transaction(transaction: &VersionedTransaction) -> ProtoSanitizedTransaction {
        let message = &transaction.message;
        let header = message.header();
        // The fixture format only distinguishes legacy from v0.
        let (is_legacy, address_table_lookups) = match message {
            VersionedMessage::Legacy(_) => (true, vec![]),
            VersionedMessage::V0(message) => (
                false,
                message
                    .address_table_lookups
                    .iter()
                    .map(|lookup| ProtoMessageAddressTableLookup {
                        account_key: lookup.account_key.to_bytes().to_vec(),
                        writable_indexes: lookup
                            .writable_indexes
                            .iter()
                            .copied()
                            .map(u32::from)
                            .collect(),
                        readonly_indexes: lookup
                            .readonly_indexes
                            .iter()
                            .copied()
                            .map(u32::from)
                            .collect(),
                    })
                    .collect(),
            ),
            VersionedMessage::V1(_) => panic!("v1 messages have no fixture representation"),
        };

        ProtoSanitizedTransaction {
            message: Some(ProtoTransactionMessage {
                is_legacy,
                header: Some(ProtoMessageHeader {
                    num_required_signatures: u32::from(header.num_required_signatures),
                    num_readonly_signed_accounts: u32::from(header.num_readonly_signed_accounts),
                    num_readonly_unsigned_accounts: u32::from(
                        header.num_readonly_unsigned_accounts,
                    ),
                }),
                account_keys: message
                    .static_account_keys()
                    .iter()
                    .map(|key| key.to_bytes().to_vec())
                    .collect(),
                recent_blockhash: message.recent_blockhash().to_bytes().to_vec(),
                instructions: message
                    .instructions()
                    .iter()
                    .map(|instruction| ProtoCompiledInstruction {
                        program_id_index: u32::from(instruction.program_id_index),
                        accounts: instruction
                            .accounts
                            .iter()
                            .copied()
                            .map(u32::from)
                            .collect(),
                        data: instruction.data.clone(),
                    })
                    .collect(),
                address_table_lookups,
            }),
            message_hash: vec![0; 32],
            signatures: transaction
                .signatures
                .iter()
                .map(|signature| signature.as_ref().to_vec())
                .collect(),
        }
    }

    fn txn_context(
        accounts: Vec<(Pubkey, AccountSharedData)>,
        transaction: VersionedTransaction,
        blockhash_queue: Vec<ProtoBlockhashQueueEntry>,
    ) -> ProtoTxnContext {
        ProtoTxnContext {
            tx: Some(proto_transaction(&transaction)),
            account_shared_data: accounts
                .into_iter()
                .map(|(pubkey, account)| account_to_proto((pubkey, account.into())))
                .collect(),
            bank: Some(ProtoTxnBank {
                blockhash_queue,
                rbh_lamports_per_signature: LAMPORTS_PER_SIGNATURE as u32,
                // Only the per-signature fee matters here; targets and burn are zeroed.
                fee_rate_governor: Some(ProtoFeeRateGovernor::default()),
                total_epoch_stake: 0,
                features: Some(proto_feature_set()),
            }),
        }
    }

    fn account(lamports: u64, data: Vec<u8>, owner: Pubkey, executable: bool) -> AccountSharedData {
        AccountSharedData::create_from_existing_shared_data(
            lamports,
            Arc::new(data),
            owner,
            executable,
            u64::MAX,
        )
    }

    fn empty_account(lamports: u64) -> AccountSharedData {
        account(lamports, vec![], Pubkey::default(), false)
    }

    fn sysvar_account<T: serde::Serialize>(id: Pubkey, state: &T) -> (Pubkey, AccountSharedData) {
        (
            id,
            account(
                1,
                bincode::serialize(state).unwrap(),
                native_loader::id(),
                false,
            ),
        )
    }

    fn clock_sysvar_account() -> (Pubkey, AccountSharedData) {
        let clock = Clock {
            slot: 20,
            epoch_start_timestamp: 1720556855,
            epoch: 0,
            leader_schedule_epoch: 1,
            unix_timestamp: 1720556855,
        };
        sysvar_account(sysvar::clock::id(), &clock)
    }

    fn epoch_schedule_sysvar_account() -> (Pubkey, AccountSharedData) {
        let epoch_schedule = EpochSchedule {
            slots_per_epoch: 432000,
            leader_schedule_slot_offset: 432000,
            warmup: true,
            first_normal_epoch: 14,
            first_normal_slot: 524256,
        };
        sysvar_account(sysvar::epoch_schedule::id(), &epoch_schedule)
    }

    fn rent_sysvar_account() -> (Pubkey, AccountSharedData) {
        sysvar_account(sysvar::rent::id(), &solana_rent::Rent::default())
    }

    fn slot_hashes_sysvar_account() -> (Pubkey, AccountSharedData) {
        (
            sysvar::slot_hashes::id(),
            account(
                1,
                wincode::serialize(&SlotHashes::default()).unwrap(),
                native_loader::id(),
                false,
            ),
        )
    }

    fn system_program_account() -> (Pubkey, AccountSharedData) {
        (
            solana_sdk_ids::system_program::id(),
            account(1, vec![], native_loader::id(), true),
        )
    }

    fn load_program(name: &str) -> Vec<u8> {
        let mut dir = env::current_dir().unwrap();
        dir.push("..");
        dir.push("svm");
        dir.push("tests");
        dir.push("example-programs");
        dir.push(name);
        dir.push(format!("{}_program.so", name.replace('-', "_")));
        fs::read(&dir).expect("program file not found")
    }

    /// Build the program + programdata accounts for an upgradeable BPF program.
    fn deploy_program(name: &str) -> [(Pubkey, AccountSharedData); 2] {
        let program_account = Pubkey::new_unique();
        let program_data_account = Pubkey::new_unique();

        let state = UpgradeableLoaderState::Program {
            programdata_address: program_data_account,
        };
        let program = account(
            25,
            bincode::serialize(&state).unwrap(),
            bpf_loader_upgradeable::id(),
            true,
        );

        let state = UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: None,
        };
        let mut header = bincode::serialize(&state).unwrap();
        let mut complement = vec![
            0;
            UpgradeableLoaderState::size_of_programdata_metadata()
                .saturating_sub(header.len())
        ];
        let mut buffer = load_program(name);
        header.append(&mut complement);
        header.append(&mut buffer);
        let program_data = account(25, header, bpf_loader_upgradeable::id(), false);

        [
            (program_account, program),
            (program_data_account, program_data),
        ]
    }

    /// Lamports of the writable account `pubkey` after execution.
    fn writable_account_lamports(result: &ProtoTxnResult, pubkey: &Pubkey) -> Option<u64> {
        result
            .modified_accounts
            .iter()
            .find(|account| account.address.as_slice() == pubkey.as_ref())
            .map(|account| account.lamports)
    }

    fn assert_executed_ok(result: &ProtoTxnResult) {
        assert!(result.executed, "transaction was not processed");
        assert_eq!(result.txn_error, 0, "transaction failed: {result:?}");
    }

    fn sanitized_message_with_program(program_id: Pubkey) -> solana_message::SanitizedMessage {
        solana_message::SanitizedMessage::try_from_legacy_message(
            legacy::Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                account_keys: vec![Pubkey::new_unique(), program_id],
                recent_blockhash: Hash::default(),
                instructions: vec![CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![],
                    data: vec![],
                }],
            },
            &HashSet::default(),
        )
        .unwrap()
    }

    #[test]
    fn noop_transaction_effects() {
        const COMPUTE_UNIT_LIMIT: u64 = 123_456;
        const LOADED_ACCOUNTS_BYTES_LIMIT: u32 = 654_321;
        let validation_error = solana_transaction_error::TransactionError::AccountNotFound;
        let processed =
            ProcessedTransaction::NoOp(Box::new(solana_svm::account_loader::NoOpTransaction {
                validation_error: validation_error.clone(),
                fee_payer_balance: Some(42),
                compute_unit_limit: COMPUTE_UNIT_LIMIT,
                loaded_accounts_bytes_limit: LOADED_ACCOUNTS_BYTES_LIMIT,
                nonce_address: None,
            }));

        let effects = super::processed_transaction_effects(
            &processed,
            &sanitized_message_with_program(Pubkey::new_unique()),
        );
        // A NoOp reports the full limit as consumed, so nothing is left over.
        assert_eq!(effects.cu_avail, 0);

        let result = super::ProtoTxnResult::from(effects);

        assert!(result.executed);
        assert_eq!(
            result.txn_error,
            solana_svm::conformance::err::serialized_error_code(&validation_error)
        );
        assert_eq!(result.instruction_error, 0);
        assert_eq!(result.instruction_error_index, 0);
        assert_eq!(result.custom_error, 0);
        assert_eq!(result.executed_units, COMPUTE_UNIT_LIMIT);
        assert_eq!(
            result.loaded_accounts_data_size,
            u64::from(LOADED_ACCOUNTS_BYTES_LIMIT)
        );
        let fee_details = result.fee_details.unwrap();
        assert_eq!(fee_details.transaction_fee, 0);
        assert_eq!(fee_details.prioritization_fee, 0);
        assert!(result.modified_accounts.is_empty());
        assert!(result.rollback_accounts.is_empty());
        assert!(result.return_data.is_empty());
    }

    #[test]
    fn test_txn_execute_clock() {
        let [(program_id, program), (program_data_id, program_data)] =
            deploy_program("clock-sysvar");
        let fee_payer = Pubkey::new_unique();
        let (blockhash_queue, recent_blockhash) = proto_blockhash_queue();

        let message = VersionedMessage::Legacy(legacy::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            account_keys: vec![fee_payer, program_id],
            recent_blockhash,
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![],
                data: vec![],
            }],
        });
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default()],
            message,
        };

        let accounts = vec![
            (fee_payer, empty_account(80000000)),
            (program_id, program),
            (program_data_id, program_data),
            clock_sysvar_account(),
            epoch_schedule_sysvar_account(),
            rent_sysvar_account(),
        ];

        let result = execute_txn_proto(&txn_context(accounts, transaction, blockhash_queue));

        assert_executed_ok(&result);
        assert_eq!(result.return_data.len(), 8);
    }

    #[test]
    fn test_simple_transfer() {
        let [(program_id, program), (program_data_id, program_data)] =
            deploy_program("simple-transfer");
        let fee_payer = Pubkey::new_unique();
        let sender = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let (blockhash_queue, recent_blockhash) = proto_blockhash_queue();

        let message = VersionedMessage::V0(v0::Message {
            header: MessageHeader {
                num_required_signatures: 2,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![fee_payer, sender, recipient, program_id, Pubkey::default()],
            recent_blockhash,
            instructions: vec![CompiledInstruction {
                program_id_index: 3,
                accounts: vec![1, 2, 4],
                data: vec![0, 0, 0, 0, 0, 0, 0, 10],
            }],
            address_table_lookups: vec![],
        });
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(), Signature::default()],
            message,
        };

        let accounts = vec![
            (fee_payer, empty_account(10000000)),
            (recipient, empty_account(900000)),
            (sender, empty_account(900000)),
            (program_id, program),
            (program_data_id, program_data),
            system_program_account(),
            clock_sysvar_account(),
            epoch_schedule_sysvar_account(),
            rent_sysvar_account(),
            slot_hashes_sysvar_account(),
        ];

        let result = execute_txn_proto(&txn_context(accounts, transaction, blockhash_queue));

        assert_executed_ok(&result);
        assert_eq!(writable_account_lamports(&result, &sender), Some(899990));
        assert_eq!(writable_account_lamports(&result, &recipient), Some(900010));
    }

    #[test]
    fn test_lookup_table() {
        let [(program_id, program), (program_data_id, program_data)] =
            deploy_program("complex-transfer");
        let fee_payer = Pubkey::new_unique();
        let sender = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let extra_account = Pubkey::new_unique();
        let (blockhash_queue, recent_blockhash) = proto_blockhash_queue();

        // The program adds this account's little-endian amount to the transfer.
        let extra_data = account(2, vec![5, 0, 0, 0, 0, 0, 0, 0], Pubkey::default(), false);

        // `recipient` and `extra_account` are supplied via the address lookup table.
        let alut_key = Pubkey::new_from_array([1; 32]);
        let alut = AddressLookupTable {
            meta: LookupTableMeta::default(),
            addresses: Cow::Owned(vec![recipient, extra_account]),
        };
        let alut_account = account(
            1,
            alut.serialize_for_tests().unwrap(),
            solana_sdk_ids::address_lookup_table::id(),
            false,
        );

        let message = VersionedMessage::V0(v0::Message {
            header: MessageHeader {
                num_required_signatures: 2,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 2,
            },
            account_keys: vec![fee_payer, sender, program_id, Pubkey::default()],
            recent_blockhash,
            // sender (1), recipient (4, ALUT), system (3), extra_account (5, ALUT)
            instructions: vec![CompiledInstruction {
                program_id_index: 2,
                accounts: vec![1, 4, 3, 5],
                data: vec![0, 0, 0, 0, 0, 0, 0, 10],
            }],
            address_table_lookups: vec![MessageAddressTableLookup {
                account_key: alut_key,
                writable_indexes: vec![0],
                readonly_indexes: vec![1],
            }],
        });
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(), Signature::default()],
            message,
        };

        let accounts = vec![
            (fee_payer, empty_account(10000000)),
            (recipient, empty_account(900000)),
            (sender, empty_account(900000)),
            (program_id, program),
            (program_data_id, program_data),
            (extra_account, extra_data),
            (alut_key, alut_account),
            system_program_account(),
            clock_sysvar_account(),
            epoch_schedule_sysvar_account(),
            rent_sysvar_account(),
            slot_hashes_sysvar_account(),
        ];

        let result = execute_txn_proto(&txn_context(accounts, transaction, blockhash_queue));

        assert_executed_ok(&result);
        assert_eq!(writable_account_lamports(&result, &sender), Some(899985));
        assert_eq!(writable_account_lamports(&result, &recipient), Some(900015));
    }
}
