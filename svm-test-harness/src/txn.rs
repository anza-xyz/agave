//! Solana SVM test harness for transactions.

use {
    crate::{
        fixture::{
            proto::{AcctState as ProtoAcctState, ResultingState as ProtoResultingState},
            txn_context::TxnContext,
            txn_result::{transaction_error_to_err_nums, FeeDetails, ResultingState, TxnResult},
        },
        program_cache::register_builtins_on_processor,
    },
    agave_feature_set::raise_cpi_nesting_limit_to_8,
    agave_precompiles::get_precompile,
    ahash::AHashSet,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::Slot,
    solana_compute_budget::compute_budget_limits::ComputeBudgetLimits,
    solana_epoch_schedule::EpochSchedule,
    solana_message::{SanitizedMessage, SimpleAddressLoader},
    solana_program_runtime::{
        execution_budget::MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
        loaded_programs::{BlockRelation, ForkGraph},
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk_ids::{address_lookup_table, bpf_loader_upgradeable, compute_budget, config, stake},
    solana_svm::{
        account_loader::CheckedTransactionDetails,
        transaction_processing_result::{
            ProcessedTransaction, TransactionProcessingResultExtensions,
        },
        transaction_processor::{
            ExecutionRecordingConfig, TransactionBatchProcessor, TransactionProcessingConfig,
            TransactionProcessingEnvironment,
        },
    },
    solana_svm_callback::{AccountState, InvokeContextCallback, TransactionProcessingCallback},
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_sysvar_id::SysvarId,
    solana_transaction::sanitized::MessageHash,
    solana_transaction_context::transaction_accounts::KeyedAccountSharedData,
    std::{
        collections::{HashMap, HashSet},
        num::NonZeroU32,
        sync::{Arc, RwLock},
    },
};

struct StubForkGraph;

impl ForkGraph for StubForkGraph {
    fn relationship(&self, _a: Slot, _b: Slot) -> BlockRelation {
        BlockRelation::Unknown
    }
}

struct TxnAccountStore {
    accounts: HashMap<Pubkey, AccountSharedData>,
}

impl InvokeContextCallback for TxnAccountStore {}

impl TransactionProcessingCallback for TxnAccountStore {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        self.accounts.get(pubkey).map(|a| (a.clone(), 0))
    }

    fn inspect_account(&self, _address: &Pubkey, _account_state: AccountState, _is_writable: bool) {
    }
}

fn get_sysvar<T: serde::de::DeserializeOwned + Default>(
    accounts: &HashMap<&[u8], &crate::fixture::proto::AcctState>,
    sysvar_id: &[u8],
) -> T {
    accounts
        .get(sysvar_id)
        .and_then(|account| bincode::deserialize(&account.data).ok())
        .unwrap_or_default()
}

fn get_dummy_bpf_native_programs() -> Vec<(Pubkey, AccountSharedData)> {
    vec![
        (
            address_lookup_table::id(),
            AccountSharedData::new(1u64, 0, &bpf_loader_upgradeable::id()),
        ),
        (
            config::id(),
            AccountSharedData::new(1u64, 0, &bpf_loader_upgradeable::id()),
        ),
        (
            stake::id(),
            AccountSharedData::new(1u64, 0, &bpf_loader_upgradeable::id()),
        ),
    ]
}

impl From<&crate::fixture::proto::AcctState> for AccountSharedData {
    fn from(value: &crate::fixture::proto::AcctState) -> Self {
        let owner = Pubkey::try_from(value.owner.as_slice()).unwrap_or_default();
        let mut account = AccountSharedData::new(value.lamports, value.data.len(), &owner);
        account.set_data_from_slice(&value.data);
        account.set_executable(value.executable);
        account
    }
}

impl From<KeyedAccountSharedData> for ProtoAcctState {
    fn from(value: KeyedAccountSharedData) -> ProtoAcctState {
        ProtoAcctState {
            address: value.0.to_bytes().to_vec(),
            lamports: value.1.lamports(),
            data: value.1.data().to_vec(),
            executable: value.1.executable(),
            owner: value.1.owner().to_bytes().to_vec(),
        }
    }
}

fn process_compute_budget_instructions<'a>(
    instructions: impl Iterator<
        Item = (
            &'a Pubkey,
            solana_svm_transaction::instruction::SVMInstruction<'a>,
        ),
    >,
) -> Result<ComputeBudgetLimits, solana_transaction_error::TransactionError> {
    let mut loaded_accounts_data_size_limit = None;

    for (program_id, instruction) in instructions {
        if *program_id == compute_budget::id()
            && instruction.data.len() >= 5
            && instruction.data[0] == 4
        {
            let size = u32::from_le_bytes([
                instruction.data[1],
                instruction.data[2],
                instruction.data[3],
                instruction.data[4],
            ]);
            loaded_accounts_data_size_limit = Some(size);
        }
    }

    let loaded_accounts_bytes = if let Some(requested_loaded_accounts_data_size_limit) =
        loaded_accounts_data_size_limit
    {
        NonZeroU32::new(requested_loaded_accounts_data_size_limit)
            .ok_or(solana_transaction_error::TransactionError::InvalidLoadedAccountsDataSizeLimit)?
    } else {
        MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES
    }
    .min(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES);

    Ok(ComputeBudgetLimits {
        loaded_accounts_bytes,
        ..Default::default()
    })
}

fn output_txn_result_from_processing_results(
    processing_results: &[solana_svm::transaction_processing_result::TransactionProcessingResult],
    sanitized_message: &SanitizedMessage,
) -> TxnResult {
    let execution_results = &processing_results[0];
    let (
        is_ok,
        sanitization_error,
        status,
        instruction_error,
        instruction_error_index,
        custom_error,
        executed_units,
        return_data,
        fee_details,
        loaded_accounts_data_size,
        resulting_state,
    ) = match execution_results {
        Ok(txn) => {
            let (status, instr_err, custom_err, instr_err_idx) =
                match txn.status().as_ref().map_err(transaction_error_to_err_nums) {
                    Ok(_) => (0, 0, 0, 0),
                    Err((status, instr_err, custom_err, instr_err_idx)) => {
                        let custom_err_ret = sanitized_message
                            .instructions()
                            .get(instr_err_idx as usize)
                            .and_then(|instr| {
                                sanitized_message
                                    .account_keys()
                                    .get(instr.program_id_index as usize)
                                    .map(|program_id| {
                                        if get_precompile(program_id, |_| true).is_some() {
                                            0
                                        } else {
                                            custom_err
                                        }
                                    })
                            })
                            .unwrap_or(custom_err);
                        (status, instr_err, custom_err_ret, instr_err_idx)
                    }
                };
            let resulting_state: Option<ProtoResultingState> = match txn {
                ProcessedTransaction::Executed(executed_tx) => {
                    let acct_states: Vec<ProtoAcctState> = executed_tx
                        .loaded_transaction
                        .accounts
                        .iter()
                        .cloned()
                        .map(Into::into)
                        .collect();
                    Some(ProtoResultingState {
                        acct_states,
                        rent_debits: vec![],
                        transaction_rent: 0,
                    })
                }
                ProcessedTransaction::FeesOnly(tx) => Some(ProtoResultingState {
                    acct_states: tx
                        .rollback_accounts
                        .iter()
                        .map(|(pubkey, acct)| ProtoAcctState {
                            address: pubkey.to_bytes().to_vec(),
                            lamports: acct.lamports(),
                            data: acct.data().to_vec(),
                            executable: acct.executable(),
                            owner: acct.owner().to_bytes().to_vec(),
                        })
                        .collect(),
                    rent_debits: vec![],
                    transaction_rent: 0,
                }),
            };
            let return_data = match txn {
                ProcessedTransaction::Executed(executed_tx) => executed_tx
                    .execution_details
                    .return_data
                    .as_ref()
                    .map(|info| info.clone().data)
                    .unwrap_or_default(),
                ProcessedTransaction::FeesOnly(_) => vec![],
            };
            (
                execution_results.was_processed_with_successful_result(),
                false,
                status,
                instr_err,
                instr_err_idx,
                custom_err,
                txn.executed_units(),
                return_data,
                Some(txn.fee_details()),
                txn.loaded_accounts_data_size(),
                resulting_state,
            )
        }
        Err(transaction_error) => {
            let (status, instr_err, custom_err, instr_err_idx) =
                transaction_error_to_err_nums(transaction_error);
            (
                false,
                true,
                status,
                instr_err,
                instr_err_idx,
                custom_err,
                0,
                vec![],
                None,
                0,
                None,
            )
        }
    };

    TxnResult {
        executed: execution_results.was_processed(),
        sanitization_error,
        resulting_state: resulting_state.map(|state| ResultingState {
            acct_states: state
                .acct_states
                .into_iter()
                .map(|acct| {
                    let pubkey = Pubkey::try_from(acct.address.as_slice()).unwrap_or_default();
                    let owner = Pubkey::try_from(acct.owner.as_slice()).unwrap_or_default();
                    let account = solana_account::Account {
                        lamports: acct.lamports,
                        data: acct.data,
                        owner,
                        executable: acct.executable,
                        rent_epoch: u64::MAX,
                    };
                    (pubkey, account)
                })
                .collect(),
        }),
        is_ok,
        status,
        instruction_error,
        instruction_error_index,
        custom_error,
        return_data,
        executed_units,
        fee_details: fee_details.map(|fees| FeeDetails {
            transaction_fee: fees.transaction_fee(),
            prioritization_fee: fees.prioritization_fee(),
        }),
        loaded_accounts_data_size: loaded_accounts_data_size as u64,
    }
}

const LAMPORTS_PER_SIGNATURE: u64 = 5000;

#[allow(deprecated)]
pub fn execute_transaction(
    context: TxnContext,
    proto_context: &crate::fixture::proto::TxnContext,
) -> Option<TxnResult> {
    let feature_set = context.epoch_context.features.clone();
    let svm_feature_set = feature_set.runtime_features();

    let slot = if context.slot_context.slot == 0 {
        10
    } else {
        context.slot_context.slot
    };
    let epoch = slot / 432000;

    let sysvar_accounts: HashMap<&[u8], &crate::fixture::proto::AcctState> = proto_context
        .account_shared_data
        .iter()
        .filter(|item| item.lamports > 0)
        .map(|item| (item.address.as_slice(), item))
        .collect();

    let rent: Rent = get_sysvar(&sysvar_accounts, solana_sysvar::rent::id().as_ref());
    let epoch_schedule: EpochSchedule = get_sysvar(
        &sysvar_accounts,
        solana_sysvar::epoch_schedule::id().as_ref(),
    );

    let mut accounts: HashMap<Pubkey, AccountSharedData> = HashMap::new();

    for (pubkey, account) in &context.accounts {
        accounts.insert(*pubkey, AccountSharedData::from(account.clone()));
    }

    for (pubkey, account) in get_dummy_bpf_native_programs() {
        accounts.insert(pubkey, account);
    }

    // Configure sysvars
    let clock = solana_clock::Clock {
        slot,
        epoch_start_timestamp: 0,
        epoch,
        leader_schedule_epoch: epoch,
        unix_timestamp: 0,
    };
    let mut clock_account = AccountSharedData::default();
    clock_account.set_data_from_slice(&bincode::serialize(&clock).unwrap());
    accounts.insert(solana_clock::Clock::id(), clock_account);

    let mut rent_account = AccountSharedData::default();
    rent_account.set_data_from_slice(&bincode::serialize(&rent).unwrap());
    accounts.insert(Rent::id(), rent_account);

    let mut epoch_schedule_account = AccountSharedData::default();
    epoch_schedule_account.set_data_from_slice(&bincode::serialize(&epoch_schedule).unwrap());
    accounts.insert(EpochSchedule::id(), epoch_schedule_account);

    #[allow(deprecated)]
    {
        use solana_sysvar::recent_blockhashes::{Entry as BlockhashesEntry, RecentBlockhashes};
        let recent_blockhashes = vec![BlockhashesEntry::default()];
        let mut recent_blockhashes_account = AccountSharedData::default();
        recent_blockhashes_account
            .set_data_from_slice(&bincode::serialize(&recent_blockhashes).unwrap());
        accounts.insert(RecentBlockhashes::id(), recent_blockhashes_account);
    }

    let fork_graph = Arc::new(RwLock::new(StubForkGraph));
    let batch_processor = TransactionBatchProcessor::new_uninitialized(slot, epoch);
    batch_processor
        .global_program_cache
        .write()
        .unwrap()
        .set_fork_graph(Arc::downgrade(&fork_graph));

    let mut account_store = TxnAccountStore {
        accounts: accounts.clone(),
    };

    register_builtins_on_processor(&batch_processor, &mut account_store.accounts, &feature_set);

    batch_processor.fill_missing_sysvar_cache_entries(&account_store);

    let blockhash = context.blockhash_queue.first().cloned().unwrap_or_default();

    let reserved_account_keys = HashSet::new();
    let runtime_transaction = match RuntimeTransaction::try_create(
        context.transaction.clone(),
        MessageHash::Compute,
        None,
        SimpleAddressLoader::Disabled,
        &reserved_account_keys,
        true,
    ) {
        Ok(tx) => tx,
        Err(e) => {
            let (status, instruction_error, _custom_error, instruction_error_index) =
                transaction_error_to_err_nums(&e);
            return Some(TxnResult {
                executed: false,
                sanitization_error: true,
                resulting_state: None,
                is_ok: false,
                status,
                instruction_error,
                instruction_error_index,
                custom_error: 0,
                return_data: vec![],
                executed_units: 0,
                fee_details: None,
                loaded_accounts_data_size: 0,
            });
        }
    };

    let compute_budget_limits = match process_compute_budget_instructions(
        SVMStaticMessage::program_instructions_iter(&runtime_transaction),
    ) {
        Ok(limits) => limits,
        Err(e) => {
            let (status, instruction_error, _custom_error, instruction_error_index) =
                transaction_error_to_err_nums(&e);
            return Some(TxnResult {
                executed: false,
                sanitization_error: true,
                resulting_state: None,
                is_ok: false,
                status,
                instruction_error,
                instruction_error_index,
                custom_error: 0,
                return_data: vec![],
                executed_units: 0,
                fee_details: None,
                loaded_accounts_data_size: 0,
            });
        }
    };

    let signature_count = runtime_transaction
        .num_transaction_signatures()
        .saturating_add(runtime_transaction.num_ed25519_signatures())
        .saturating_add(runtime_transaction.num_secp256k1_signatures())
        .saturating_add(runtime_transaction.num_secp256r1_signatures());

    let fee_details = solana_fee_structure::FeeDetails::new(
        signature_count.saturating_mul(LAMPORTS_PER_SIGNATURE),
        compute_budget_limits.get_prioritization_fee(),
    );

    let raise_cpi_limit = feature_set.is_active(&raise_cpi_nesting_limit_to_8::id());
    let compute_budget = compute_budget_limits.get_compute_budget_and_limits(
        compute_budget_limits.loaded_accounts_bytes,
        fee_details,
        raise_cpi_limit,
    );

    let check_result = Ok(CheckedTransactionDetails::new(None, compute_budget));

    let processing_environment = TransactionProcessingEnvironment {
        blockhash,
        feature_set: svm_feature_set,
        blockhash_lamports_per_signature: LAMPORTS_PER_SIGNATURE,
        program_runtime_environments_for_execution: batch_processor
            .get_environments_for_epoch(epoch),
        program_runtime_environments_for_deployment: batch_processor
            .get_environments_for_epoch(epoch),
        ..TransactionProcessingEnvironment::default()
    };

    let recording_config = ExecutionRecordingConfig {
        enable_cpi_recording: false,
        enable_log_recording: true,
        enable_return_data_recording: true,
        enable_transaction_balance_recording: false,
    };

    let processing_config = TransactionProcessingConfig {
        account_overrides: None,
        check_program_modification_slot: false,
        log_messages_bytes_limit: None,
        limit_to_load_programs: true,
        recording_config,
        drop_on_failure: false,
        all_or_nothing: false,
    };

    let transactions = vec![runtime_transaction];
    let check_results = vec![check_result];

    let batch_output = batch_processor.load_and_execute_sanitized_transactions(
        &account_store,
        &transactions,
        check_results,
        &processing_environment,
        &processing_config,
    );

    let runtime_transaction_ref = &transactions[0];

    let account_keys = proto_context
        .tx
        .as_ref()
        .and_then(|tx| tx.message.as_ref())
        .map(|message| message.account_keys.clone())
        .unwrap_or_default();

    let mut txn_result = output_txn_result_from_processing_results(
        &batch_output.processing_results,
        runtime_transaction_ref.message(),
    );
    if let Some(relevant_accounts) = &mut txn_result.resulting_state {
        let mut loaded_account_keys = AHashSet::<Pubkey>::new();
        loaded_account_keys.extend(
            account_keys
                .iter()
                .filter_map(|key| Pubkey::try_from(key.as_slice()).ok()),
        );
        match runtime_transaction_ref.message() {
            SanitizedMessage::Legacy(_) => {}
            SanitizedMessage::V0(message) => {
                loaded_account_keys.extend(message.loaded_addresses.writable.clone().iter());
                loaded_account_keys.extend(message.loaded_addresses.readonly.clone().iter());
            }
        }

        relevant_accounts.acct_states = relevant_accounts
            .acct_states
            .clone()
            .into_iter()
            .enumerate()
            .filter(|&(i, _)| runtime_transaction_ref.message().is_writable(i))
            .map(|(_, account)| account)
            .collect();

        relevant_accounts
            .acct_states
            .retain(|(pubkey, _)| loaded_account_keys.contains(pubkey));
    }

    Some(txn_result)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::fixture::{
            proto::{
                AcctState as ProtoAcctState, CompiledInstruction as ProtoCompiledInstruction,
                MessageHeader as ProtoMessageHeader, SanitizedTransaction as ProtoSanitizedTx,
                SlotContext as ProtoSlotContext, TransactionMessage as ProtoTransactionMessage,
                TxnContext as ProtoTxnContext,
            },
            txn_context::TxnContext,
        },
        solana_hash::Hash,
        solana_pubkey::Pubkey,
        solana_sdk_ids::native_loader,
        solana_signature::Signature,
        solana_sysvar_id::SysvarId,
    };

    fn get_clock_sysvar_account() -> ProtoAcctState {
        let clock = solana_clock::Clock {
            slot: 20,
            epoch_start_timestamp: 1720556855,
            epoch: 0,
            leader_schedule_epoch: 1,
            unix_timestamp: 1720556855,
        };
        ProtoAcctState {
            address: solana_clock::Clock::id().to_bytes().to_vec(),
            lamports: 1,
            data: bincode::serialize(&clock).unwrap(),
            executable: false,
            owner: native_loader::id().to_bytes().to_vec(),
        }
    }

    fn get_epoch_schedule_sysvar_account() -> ProtoAcctState {
        let epoch_schedule = EpochSchedule {
            slots_per_epoch: 432000,
            leader_schedule_slot_offset: 432000,
            warmup: true,
            first_normal_epoch: 14,
            first_normal_slot: 524256,
        };
        ProtoAcctState {
            address: solana_epoch_schedule::EpochSchedule::id()
                .to_bytes()
                .to_vec(),
            lamports: 1,
            data: bincode::serialize(&epoch_schedule).unwrap(),
            executable: false,
            owner: native_loader::id().to_bytes().to_vec(),
        }
    }

    fn get_rent_sysvar_account() -> ProtoAcctState {
        let rent = Rent {
            lamports_per_byte_year: 3480,
            exemption_threshold: 2.0,
            burn_percent: 50,
        };
        ProtoAcctState {
            address: solana_rent::Rent::id().to_bytes().to_vec(),
            lamports: 1,
            data: bincode::serialize(&rent).unwrap(),
            executable: false,
            owner: native_loader::id().to_bytes().to_vec(),
        }
    }

    #[test]
    fn test_system_transfer() {
        let clock_sysvar = get_clock_sysvar_account();
        let epoch_schedule = get_epoch_schedule_sysvar_account();
        let rent_sysvar = get_rent_sysvar_account();

        let fee_payer = Pubkey::new_unique();
        let fee_payer_data = ProtoAcctState {
            address: fee_payer.to_bytes().to_vec(),
            lamports: 10_000_000,
            data: vec![],
            executable: false,
            owner: vec![0; 32],
        };

        let recipient = Pubkey::new_unique();
        let recipient_data = ProtoAcctState {
            address: recipient.to_bytes().to_vec(),
            lamports: 890_880,
            data: vec![],
            executable: false,
            owner: vec![0; 32],
        };

        let header = ProtoMessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 1,
        };

        let transfer_amount: u64 = 1000;
        let mut instr_data = vec![2, 0, 0, 0];
        instr_data.extend_from_slice(&transfer_amount.to_le_bytes());

        let instr = ProtoCompiledInstruction {
            program_id_index: 2,
            accounts: vec![0, 1],
            data: instr_data,
        };

        let blockhash = Hash::new_unique();
        let blockhash_queue = vec![blockhash.to_bytes().to_vec()];

        let message = ProtoTransactionMessage {
            is_legacy: true,
            header: Some(header),
            account_keys: vec![
                fee_payer.to_bytes().to_vec(),
                recipient.to_bytes().to_vec(),
                vec![0; 32],
            ],
            recent_blockhash: blockhash.to_bytes().to_vec(),
            instructions: vec![instr],
            address_table_lookups: vec![],
        };

        let tx = ProtoSanitizedTx {
            message: Some(message),
            message_hash: Hash::new_unique().to_bytes().to_vec(),
            signatures: vec![Signature::default().as_ref().to_vec()],
        };

        let slot_ctx = ProtoSlotContext {
            slot: 20,
            block_height: 0,
            poh: vec![],
            parent_bank_hash: vec![],
            parent_lthash: vec![],
            prev_slot: 0,
            prev_lps: 0,
            prev_epoch_capitalization: 0,
            fee_rate_governor: None,
            parent_signature_count: 0,
        };

        let proto_context = ProtoTxnContext {
            tx: Some(tx),
            account_shared_data: vec![
                fee_payer_data,
                recipient_data,
                clock_sysvar,
                epoch_schedule,
                rent_sysvar,
            ],
            blockhash_queue: blockhash_queue.clone(),
            epoch_ctx: None,
            slot_ctx: Some(slot_ctx),
        };

        let txn_context = TxnContext::try_from(proto_context.clone()).unwrap();
        let result = execute_transaction(txn_context, &proto_context);

        assert!(result.is_some());
        let txn_result = result.unwrap();
        assert!(txn_result.executed);
        assert!(txn_result.is_ok);

        if let Some(state) = &txn_result.resulting_state {
            let fee_payer_result = state
                .acct_states
                .iter()
                .find(|(k, _)| *k == fee_payer)
                .map(|(_, v)| v);
            assert!(fee_payer_result.is_some());
            let fee_payer_account = fee_payer_result.unwrap();
            assert!(fee_payer_account.lamports < 10_000_000 - transfer_amount);

            let recipient_result = state
                .acct_states
                .iter()
                .find(|(k, _)| *k == recipient)
                .map(|(_, v)| v);
            assert!(recipient_result.is_some());
            assert_eq!(
                recipient_result.unwrap().lamports,
                890_880 + transfer_amount
            );
        } else {
            panic!("Expected resulting state");
        }
    }
}
