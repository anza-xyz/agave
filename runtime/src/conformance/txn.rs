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

#[cfg(feature = "conformance")]
use {
    super::versioned_message_from_proto,
    agave_feature_set::virtual_address_space_adjustments,
    agave_precompiles::is_precompile,
    ahash::AHashSet,
    protosol::protos::{
        self, AcctState, FeeDetails as ProtoFeeDetails, TxnContext as ProtoTxnContext,
        TxnResult as ProtoTxnResult,
    },
    solana_hash::Hash,
    solana_instruction::error::InstructionError,
    solana_message::SanitizedMessage,
    solana_signature::Signature,
    solana_svm::conformance::{
        account_state::{account_from_proto, account_to_proto},
        direct_mapping::direct_mapping_handle_cu_exhaustion,
        err::serialized_error_code,
        feature_set::feature_set_from_proto,
    },
    solana_svm::transaction_processing_result::{
        ProcessedTransaction, TransactionProcessingResultExtensions,
    },
    solana_svm::{
        account_loader::FeesOnlyTransaction, transaction_execution_result::ExecutedTransaction,
    },
};
use {
    crate::{
        bank::{Bank, BankFieldsToDeserialize, BankRc},
        epoch_stakes::VersionedEpochStakes,
        stake_history::StakeHistory,
        stakes::{DeserializableDelegationStakes, SerdeStakesToStakeFormat, Stakes},
    },
    agave_feature_set::FeatureSet,
    solana_account::AccountSharedData,
    solana_accounts_db::{
        accounts::Accounts, accounts_db::AccountsDb, ancestors::Ancestors,
        blockhash_queue::BlockhashQueue,
    },
    solana_clock::{BankId, Clock, Epoch, MAX_PROCESSING_AGE},
    solana_epoch_schedule::EpochSchedule,
    solana_fee_calculator::FeeRateGovernor,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk_ids::sysvar,
    solana_stake_interface::state::Stake,
    solana_svm::{
        conformance::setup::sysvar_from_accounts,
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_processing_result::TransactionProcessingResult,
        transaction_processor::{ExecutionRecordingConfig, TransactionProcessingConfig},
    },
    solana_svm_timings::ExecuteTimings,
    solana_transaction::{
        TransactionVerificationMode, sanitized::SanitizedTransaction,
        versioned::VersionedTransaction,
    },
    solana_transaction_error::TransactionError,
    solana_vote::vote_account::VoteAccounts,
    std::{collections::HashMap, sync::Arc},
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
        runtime_transaction: Box<RuntimeTransaction<SanitizedTransaction>>,
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
    const TICKS_PER_SLOT: u64 = 64;

    // Slot and parent slot come from the clock sysvar.
    let clock: Clock = sysvar_from_accounts(accounts, &sysvar::clock::id());
    let slot = clock.slot;
    let parent_slot = slot.saturating_sub(1);

    let epoch_schedule: EpochSchedule =
        sysvar_from_accounts(accounts, &sysvar::epoch_schedule::id());
    let epoch = epoch_schedule.get_epoch(slot);

    // Populate the accounts DB with the input accounts at the parent slot.
    let bank_accounts = Accounts::new(Arc::new(AccountsDb::default_for_tests()));
    let ancestors = Ancestors::from(vec![parent_slot]);
    bank_accounts.store_accounts_seq((parent_slot, accounts), BankId::default(), None, &ancestors);
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
        tick_height: TICKS_PER_SLOT.saturating_mul(slot),
        max_tick_height: TICKS_PER_SLOT.saturating_mul(slot.saturating_add(1)),
        ticks_per_slot: TICKS_PER_SLOT,
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

    let runtime_transaction = match bank.verify_transaction(
        transaction,
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

/// Parse the input accounts into keyed `AccountSharedData`, dropping zero-lamport
/// accounts (treated as nonexistent).
#[cfg(feature = "conformance")]
fn deserialize_accounts(accounts: &[AcctState]) -> Vec<(Pubkey, AccountSharedData)> {
    accounts
        .iter()
        .filter(|account| account.lamports > 0)
        .map(|account| {
            let (pubkey, account) = account_from_proto(account.clone());
            (pubkey, account.into())
        })
        .collect()
}

#[cfg(feature = "conformance")]
fn restore_blockhash_queue(entries: &[protos::BlockhashQueueEntry]) -> BlockhashQueue {
    let mut blockhash_queue = BlockhashQueue::default();
    for entry in entries {
        let bytes = <[u8; 32]>::try_from(entry.blockhash.as_slice()).unwrap();
        blockhash_queue.register_hash(&Hash::new_from_array(bytes), entry.lamports_per_signature);
    }
    blockhash_queue
}

/// Firedancer error numbers: the bincode-serialized enum discriminant `+ 1`.
#[cfg(feature = "conformance")]
#[derive(Default)]
struct ProtoTxnErrorFields {
    txn_error: u32,
    instruction_error: u32,
    custom_error: u32,
    instruction_error_index: u32,
}

#[cfg(feature = "conformance")]
impl ProtoTxnErrorFields {
    fn from_processed_transaction(
        txn: &ProcessedTransaction,
        sanitized_message: &SanitizedMessage,
    ) -> Self {
        match txn.status() {
            Ok(()) => Self::default(),
            Err(transaction_error) => Self::from_transaction_error(&transaction_error)
                .zero_precompile_custom_error(sanitized_message),
        }
    }

    fn from_transaction_error(transaction_error: &TransactionError) -> Self {
        let (instruction_error, custom_error, instruction_error_index) = match transaction_error {
            TransactionError::InstructionError(instruction_error_index, instruction_error) => {
                let custom_error = match instruction_error {
                    InstructionError::Custom(custom_error) => *custom_error,
                    _ => 0,
                };
                (
                    serialized_error_code(instruction_error),
                    custom_error,
                    (*instruction_error_index).into(),
                )
            }
            _ => (0, 0, 0),
        };

        Self {
            txn_error: serialized_error_code(transaction_error),
            instruction_error,
            custom_error,
            instruction_error_index,
        }
    }

    /// Firedancer does not compare precompile custom error codes because minor
    /// implementation differences can surface different custom values.
    fn zero_precompile_custom_error(mut self, sanitized_message: &SanitizedMessage) -> Self {
        // Custom error is zeroed when the failing instruction is a precompile.
        if self.custom_error != 0
            && instruction_is_precompile(self.instruction_error_index, sanitized_message)
        {
            self.custom_error = 0;
        }
        self
    }
}

#[cfg(feature = "conformance")]
fn instruction_is_precompile(
    instruction_error_index: u32,
    sanitized_message: &SanitizedMessage,
) -> bool {
    let Ok(instruction_error_index) = usize::try_from(instruction_error_index) else {
        return false;
    };

    sanitized_message
        .program_instructions_iter()
        .nth(instruction_error_index)
        .is_some_and(|(program_id, _)| is_precompile(program_id, |_| true))
}

#[cfg(feature = "conformance")]
struct ProtoTxnEffects {
    modified_accounts: Vec<AcctState>,
    rollback_accounts: Vec<AcctState>,
    return_data: Vec<u8>,
}

#[cfg(feature = "conformance")]
impl ProtoTxnEffects {
    fn from_processed_transaction(
        txn: &ProcessedTransaction,
        sanitized_message: &SanitizedMessage,
    ) -> Self {
        match txn {
            ProcessedTransaction::Executed(executed_tx) => {
                executed_transaction_effects(executed_tx, sanitized_message)
            }
            ProcessedTransaction::FeesOnly(tx) => fees_only_transaction_effects(tx),
            ProcessedTransaction::NoOp(_) => ProtoTxnEffects {
                modified_accounts: vec![],
                rollback_accounts: vec![],
                return_data: vec![],
            },
        }
    }
}

#[cfg(feature = "conformance")]
fn executed_transaction_effects(
    executed_tx: &ExecutedTransaction,
    sanitized_message: &SanitizedMessage,
) -> ProtoTxnEffects {
    let loaded = &executed_tx.loaded_transaction;
    let modified_accounts = loaded
        .accounts
        .iter()
        .enumerate()
        .filter(|(index, _)| sanitized_message.is_writable(*index))
        .map(|(_, (pubkey, account))| account_to_proto((*pubkey, account.clone().into())))
        .collect();
    let rollback_accounts = if executed_tx.execution_details.status.is_err() {
        loaded
            .rollback_accounts
            .iter()
            .map(|(pubkey, account)| account_to_proto((*pubkey, account.clone().into())))
            .collect()
    } else {
        vec![]
    };
    let return_data = executed_tx
        .execution_details
        .return_data
        .as_ref()
        .map(|info| info.data.clone())
        .unwrap_or_default();

    ProtoTxnEffects {
        modified_accounts,
        rollback_accounts,
        return_data,
    }
}

#[cfg(feature = "conformance")]
fn fees_only_transaction_effects(tx: &FeesOnlyTransaction) -> ProtoTxnEffects {
    ProtoTxnEffects {
        modified_accounts: vec![],
        rollback_accounts: tx
            .rollback_accounts
            .iter()
            .map(|(pubkey, account)| account_to_proto((*pubkey, account.clone().into())))
            .collect(),
        return_data: vec![],
    }
}

/// Map the processor's result for the single executed transaction into a
/// `TxnResult`.
#[cfg(feature = "conformance")]
fn output_txn_result(
    execution_result: &TransactionProcessingResult,
    sanitized_message: &SanitizedMessage,
) -> ProtoTxnResult {
    let executed = execution_result.was_processed();
    match execution_result {
        Ok(txn) => {
            let error = ProtoTxnErrorFields::from_processed_transaction(txn, sanitized_message);
            let effects = ProtoTxnEffects::from_processed_transaction(txn, sanitized_message);
            let fees = txn.fee_details();

            ProtoTxnResult {
                executed,
                txn_error: error.txn_error,
                instruction_error: error.instruction_error,
                instruction_error_index: error.instruction_error_index,
                custom_error: error.custom_error,
                return_data: effects.return_data,
                executed_units: txn.executed_units(),
                fee_details: Some(ProtoFeeDetails {
                    transaction_fee: fees.transaction_fee(),
                    prioritization_fee: fees.prioritization_fee(),
                }),
                loaded_accounts_data_size: u64::from(txn.loaded_accounts_data_size()),
                modified_accounts: effects.modified_accounts,
                rollback_accounts: effects.rollback_accounts,
            }
        }
        Err(transaction_error) => {
            let error = ProtoTxnErrorFields::from_transaction_error(transaction_error);
            ProtoTxnResult {
                executed,
                txn_error: error.txn_error,
                instruction_error: error.instruction_error,
                instruction_error_index: error.instruction_error_index,
                custom_error: error.custom_error,
                ..Default::default()
            }
        }
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
    let fee_rate_governor = FeeRateGovernor {
        lamports_per_signature: u64::from(txn_bank.rbh_lamports_per_signature),
        target_lamports_per_signature: input_fee_rate_governor.target_lamports_per_signature,
        target_signatures_per_slot: input_fee_rate_governor.target_signatures_per_slot,
        min_lamports_per_signature: input_fee_rate_governor.min_lamports_per_signature,
        max_lamports_per_signature: input_fee_rate_governor.max_lamports_per_signature,
        burn_percent: input_fee_rate_governor.burn_percent as u8,
    };

    let feature_set = txn_bank
        .features
        .as_ref()
        .map(feature_set_from_proto)
        .unwrap();
    let virtual_address_space_adjustments_active =
        feature_set.is_active(&virtual_address_space_adjustments::id());

    let tx = context.tx.as_ref().unwrap();
    let proto_message = tx.message.as_ref().unwrap();
    let message = versioned_message_from_proto(proto_message);
    let mut signatures = tx
        .signatures
        .iter()
        .map(|item| Signature::try_from(item.as_slice()).unwrap())
        .collect::<Vec<Signature>>();
    if signatures.is_empty() {
        // Default: a single empty signature (keeps simple cases valid).
        signatures.push(Signature::default());
    }
    let transaction = VersionedTransaction {
        signatures,
        message,
    };

    let (result, runtime_transaction) = match execute_txn(
        &accounts,
        feature_set,
        blockhash_queue,
        fee_rate_governor,
        txn_bank.total_epoch_stake,
        transaction,
    ) {
        BankTxnProcessingResult::FailedVerification(err) => {
            let error = ProtoTxnErrorFields::from_transaction_error(&err);
            return ProtoTxnResult {
                executed: false,
                txn_error: error.txn_error,
                instruction_error: error.instruction_error,
                instruction_error_index: error.instruction_error_index,
                // Precompile error codes are not conformant, so they are ignored here.
                custom_error: 0,
                ..Default::default()
            };
        }
        BankTxnProcessingResult::Processed {
            result,
            runtime_transaction,
        } => (result, runtime_transaction),
    };
    let sanitized_message = runtime_transaction.message();

    let mut txn_result = output_txn_result(&result, sanitized_message);

    let cu_avail = match &result {
        Ok(ProcessedTransaction::Executed(executed_tx)) => executed_tx
            .loaded_transaction
            .compute_budget
            .compute_unit_limit
            .saturating_sub(txn_result.executed_units),
        _ => 0,
    };
    direct_mapping_handle_cu_exhaustion(
        virtual_address_space_adjustments_active,
        cu_avail,
        txn_result.txn_error != 0,
        txn_result
            .modified_accounts
            .iter_mut()
            .map(|acc| &mut acc.data),
    );

    // Only keep modified accounts that were passed in as account keys or were
    // loaded via an address lookup table.
    let account_keys = &proto_message.account_keys;
    let mut loaded_account_keys = AHashSet::<Pubkey>::new();
    loaded_account_keys.extend(
        account_keys
            .iter()
            .map(|key| Pubkey::try_from(key.as_slice()).unwrap()),
    );
    if let SanitizedMessage::V0(message) = sanitized_message {
        loaded_account_keys.extend(message.loaded_addresses.writable.iter().copied());
        loaded_account_keys.extend(message.loaded_addresses.readonly.iter().copied());
    }
    txn_result.modified_accounts.retain(|account| {
        Pubkey::try_from(account.address.as_slice())
            .map(|pubkey| loaded_account_keys.contains(&pubkey))
            .unwrap()
    });

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

#[cfg(test)]
mod tests {
    use {
        super::{BankTxnProcessingResult, execute_txn},
        agave_feature_set::{FeatureSet, disable_sbpf_v0_execution, set_exempt_rent_epoch_max},
        solana_account::{AccountSharedData, ReadableAccount},
        solana_accounts_db::blockhash_queue::BlockhashQueue,
        solana_address_lookup_table_interface::state::{AddressLookupTable, LookupTableMeta},
        solana_clock::Clock,
        solana_epoch_schedule::EpochSchedule,
        solana_fee_calculator::FeeRateGovernor,
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
        solana_svm::transaction_processing_result::{
            ProcessedTransaction, TransactionProcessingResultExtensions,
        },
        solana_transaction::versioned::VersionedTransaction,
        std::{borrow::Cow, env, fs, sync::Arc},
    };
    #[cfg(feature = "conformance")]
    use {solana_instruction::error::InstructionError, std::collections::HashSet};

    /// All features enabled except `disable_sbpf_v0_execution`, so the v0
    /// `complex-transfer` program loads. `set_exempt_rent_epoch_max` is forced on
    /// to match the accounts' `u64::MAX` rent epoch.
    fn feature_set() -> FeatureSet {
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.activate(&set_exempt_rent_epoch_max::id(), 0);
        feature_set.deactivate(&disable_sbpf_v0_execution::id());
        feature_set
    }

    fn fee_rate_governor() -> FeeRateGovernor {
        // Mirrors the proto path: only `lamports_per_signature` is set; the
        // targets/burn are zeroed (unlike `FeeRateGovernor::default()`).
        FeeRateGovernor {
            lamports_per_signature: 5000,
            target_lamports_per_signature: 0,
            target_signatures_per_slot: 0,
            min_lamports_per_signature: 0,
            max_lamports_per_signature: 0,
            burn_percent: 0,
        }
    }

    /// A blockhash queue with two registered hashes; returns the queue plus the
    /// most-recent blockhash to use as the message's `recent_blockhash`.
    fn blockhash_queue() -> (BlockhashQueue, Hash) {
        let mut queue = BlockhashQueue::default();
        queue.register_hash(&Hash::new_unique(), 5000);
        let recent = Hash::new_unique();
        queue.register_hash(&recent, 5000);
        (queue, recent)
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

    /// Lamports of the writable account `pubkey` after execution, if the
    /// transaction executed successfully.
    fn writable_account_lamports(
        execution: &BankTxnProcessingResult,
        pubkey: &Pubkey,
    ) -> Option<u64> {
        match execution {
            BankTxnProcessingResult::Processed {
                result: Ok(ProcessedTransaction::Executed(executed_tx)),
                runtime_transaction,
            } => executed_tx
                .loaded_transaction
                .accounts
                .iter()
                .enumerate()
                .filter(|(index, _)| runtime_transaction.message().is_writable(*index))
                .find(|(_, (key, _))| key == pubkey)
                .map(|(_, (_, account))| account.lamports()),
            _ => None,
        }
    }

    fn return_data(execution: &BankTxnProcessingResult) -> Vec<u8> {
        match execution {
            BankTxnProcessingResult::Processed {
                result: Ok(ProcessedTransaction::Executed(executed_tx)),
                ..
            } => executed_tx
                .execution_details
                .return_data
                .as_ref()
                .map(|info| info.data.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn assert_executed_ok(execution: &BankTxnProcessingResult) {
        match execution {
            BankTxnProcessingResult::Processed { result, .. } => {
                assert!(result.was_processed_with_successful_result())
            }
            BankTxnProcessingResult::FailedVerification(err) => {
                panic!("transaction failed verification: {err:?}")
            }
        }
    }

    #[cfg(feature = "conformance")]
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

    #[cfg(feature = "conformance")]
    #[test]
    fn proto_txn_error_fields_zeroes_precompile_custom_error() {
        let error = solana_transaction_error::TransactionError::InstructionError(
            0,
            InstructionError::Custom(7),
        );
        let fields = super::ProtoTxnErrorFields::from_transaction_error(&error)
            .zero_precompile_custom_error(&sanitized_message_with_program(
                solana_sdk_ids::secp256k1_program::id(),
            ));

        assert_eq!(fields.custom_error, 0);
        assert_ne!(fields.instruction_error, 0);
        assert_eq!(fields.instruction_error_index, 0);
    }

    #[cfg(feature = "conformance")]
    #[test]
    fn proto_txn_error_fields_keeps_non_precompile_custom_error() {
        let error = solana_transaction_error::TransactionError::InstructionError(
            0,
            InstructionError::Custom(7),
        );
        let fields = super::ProtoTxnErrorFields::from_transaction_error(&error)
            .zero_precompile_custom_error(&sanitized_message_with_program(Pubkey::new_unique()));

        assert_eq!(fields.custom_error, 7);
        assert_ne!(fields.instruction_error, 0);
        assert_eq!(fields.instruction_error_index, 0);
    }

    #[cfg(feature = "conformance")]
    #[test]
    fn output_txn_result_handles_noop_transaction() {
        const COMPUTE_UNIT_LIMIT: u64 = 123_456;
        const LOADED_ACCOUNTS_BYTES_LIMIT: u32 = 654_321;
        let validation_error = solana_transaction_error::TransactionError::AccountNotFound;
        let processing_result = Ok(ProcessedTransaction::NoOp(Box::new(
            solana_svm::account_loader::NoOpTransaction {
                validation_error: validation_error.clone(),
                fee_payer_balance: Some(42),
                compute_unit_limit: COMPUTE_UNIT_LIMIT,
                loaded_accounts_bytes_limit: LOADED_ACCOUNTS_BYTES_LIMIT,
            },
        )));

        let result = super::output_txn_result(
            &processing_result,
            &sanitized_message_with_program(Pubkey::new_unique()),
        );

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
        let (blockhash_queue, recent_blockhash) = blockhash_queue();

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

        let execution = execute_txn(
            &accounts,
            feature_set(),
            blockhash_queue,
            fee_rate_governor(),
            0,
            transaction,
        );

        assert_executed_ok(&execution);
        assert_eq!(return_data(&execution).len(), 8);
    }

    #[test]
    fn test_simple_transfer() {
        let [(program_id, program), (program_data_id, program_data)] =
            deploy_program("simple-transfer");
        let fee_payer = Pubkey::new_unique();
        let sender = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let (blockhash_queue, recent_blockhash) = blockhash_queue();

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

        let execution = execute_txn(
            &accounts,
            feature_set(),
            blockhash_queue,
            fee_rate_governor(),
            0,
            transaction,
        );

        assert_executed_ok(&execution);
        assert_eq!(writable_account_lamports(&execution, &sender), Some(899990));
        assert_eq!(
            writable_account_lamports(&execution, &recipient),
            Some(900010)
        );
    }

    #[test]
    fn test_lookup_table() {
        let [(program_id, program), (program_data_id, program_data)] =
            deploy_program("complex-transfer");
        let fee_payer = Pubkey::new_unique();
        let sender = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let extra_account = Pubkey::new_unique();
        let (blockhash_queue, recent_blockhash) = blockhash_queue();

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

        let execution = execute_txn(
            &accounts,
            feature_set(),
            blockhash_queue,
            fee_rate_governor(),
            0,
            transaction,
        );

        assert_executed_ok(&execution);
        assert_eq!(writable_account_lamports(&execution, &sender), Some(899985));
        assert_eq!(
            writable_account_lamports(&execution, &recipient),
            Some(900015)
        );
    }
}
