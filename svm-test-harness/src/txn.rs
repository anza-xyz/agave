//! Solana SVM test harness for transactions.

use {
    crate::fixture::{
        proto::{AcctState as ProtoAcctState, ResultingState as ProtoResultingState},
        txn_context::TxnContext,
        txn_result::{transaction_error_to_err_nums, FeeDetails, ResultingState, TxnResult},
    },
    agave_precompiles::get_precompile,
    ahash::AHashSet,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_accounts_db::{
        accounts_db::AccountsDbConfig,
        accounts_file::StorageAccess,
        accounts_index::{AccountsIndexConfig, IndexLimit},
    },
    solana_clock::MAX_PROCESSING_AGE,
    solana_epoch_schedule::EpochSchedule,
    solana_genesis_config::GenesisConfig,
    solana_message::SanitizedMessage,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime::{
        bank::{Bank, LoadAndExecuteTransactionsOutput},
        bank_forks::BankForks,
        runtime_config::RuntimeConfig,
    },
    solana_sdk_ids::{address_lookup_table, bpf_loader_upgradeable, config, stake},
    solana_svm::{
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_processing_result::{
            ProcessedTransaction, TransactionProcessingResultExtensions,
        },
        transaction_processor::{ExecutionRecordingConfig, TransactionProcessingConfig},
    },
    solana_svm_timings::ExecuteTimings,
    solana_transaction::TransactionVerificationMode,
    solana_transaction_context::transaction_accounts::KeyedAccountSharedData,
    std::{
        collections::HashMap,
        num::NonZeroUsize,
        sync::{atomic::AtomicBool, Arc},
    },
};

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

fn output_txn_result_from_result(
    value: LoadAndExecuteTransactionsOutput,
    sanitized_message: &SanitizedMessage,
) -> TxnResult {
    let execution_results = &value.processing_results[0];
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

#[allow(deprecated)]
pub fn execute_transaction(
    context: TxnContext,
    proto_context: &crate::fixture::proto::TxnContext,
) -> Option<TxnResult> {
    let feature_set = context.epoch_context.features.clone();

    const FEE_COLLECTOR: Pubkey = Pubkey::from_str_const("1111111111111111111111111111111111");

    let slot = if context.slot_context.slot == 0 {
        10
    } else {
        context.slot_context.slot
    };

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

    let mut genesis_config = GenesisConfig {
        creation_time: 0,
        rent,
        epoch_schedule,
        ..GenesisConfig::default()
    };

    let bpf_native_program_accounts = get_dummy_bpf_native_programs();
    bpf_native_program_accounts
        .iter()
        .for_each(|(key, account)| {
            genesis_config.add_account(*key, account.clone());
        });

    let genesis_hash = context.blockhash_queue.first().cloned();

    let index = Some(AccountsIndexConfig {
        bins: Some(2),
        num_flush_threads: Some(NonZeroUsize::new(1).unwrap()),
        index_limit: IndexLimit::InMemOnly,
        ..AccountsIndexConfig::default()
    });

    let shm_path = std::path::PathBuf::from("/dev/shm");

    let accounts_db_config = AccountsDbConfig {
        index,
        storage_access: StorageAccess::Mmap,
        skip_initial_hash_calc: true,
        base_working_path: Some(shm_path),
        ..AccountsDbConfig::default()
    };

    let bank = Bank::new_from_genesis(
        &genesis_config,
        Arc::new(RuntimeConfig::default()),
        vec!["/dev/shm/a".into()],
        None,
        accounts_db_config,
        None,
        Some(FEE_COLLECTOR),
        Arc::new(AtomicBool::new(false)),
        genesis_hash,
        Some(feature_set.clone()),
    );
    let bank_forks = BankForks::new_rw_arc(bank);
    let mut bank = bank_forks.read().unwrap().root_bank();
    bank.rehash();

    if slot > 0 {
        let new_bank = Bank::new_from_parent(bank.clone(), &FEE_COLLECTOR, slot);
        bank = bank_forks
            .write()
            .unwrap()
            .insert(new_bank)
            .clone_without_scheduler();
        bank.prune_program_cache(slot, bank.epoch());
    }

    bank.store_account(&address_lookup_table::id(), &AccountSharedData::default());
    bank.store_account(&config::id(), &AccountSharedData::default());
    bank.store_account(&stake::id(), &AccountSharedData::default());

    bank.get_transaction_processor().reset_sysvar_cache();
    for (pubkey, account) in &context.accounts {
        let account_data = AccountSharedData::from(account.clone());
        bank.store_account(pubkey, &account_data);
    }
    bank.get_transaction_processor()
        .fill_missing_sysvar_cache_entries(bank.as_ref());

    let sysvar_recent_blockhashes = bank.get_sysvar_cache_for_tests().get_recent_blockhashes();
    let mut lamports_per_signature: Option<u64> = None;
    if let Ok(recent_blockhashes) = &sysvar_recent_blockhashes {
        if let Some(hash) = recent_blockhashes.first() {
            if hash.fee_calculator.lamports_per_signature != 0 {
                lamports_per_signature = Some(hash.fee_calculator.lamports_per_signature);
            }
        }
    }

    for blockhash in &context.blockhash_queue {
        bank.register_recent_blockhash_for_test(blockhash, lamports_per_signature);
    }
    bank.update_recent_blockhashes();
    bank.get_transaction_processor().reset_sysvar_cache();
    bank.get_transaction_processor()
        .fill_missing_sysvar_cache_entries(bank.as_ref());

    let runtime_transaction = match bank.verify_transaction(
        context.transaction.clone(),
        TransactionVerificationMode::HashAndVerifyPrecompiles,
    ) {
        Ok(v) => v,
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

    let transactions = vec![runtime_transaction];
    let batch = bank.prepare_sanitized_batch(&transactions);

    let recording_config = ExecutionRecordingConfig {
        enable_cpi_recording: false,
        enable_log_recording: true,
        enable_return_data_recording: true,
        enable_transaction_balance_recording: false,
    };

    let mut timings = ExecuteTimings::default();

    let configs = TransactionProcessingConfig {
        account_overrides: None,
        check_program_modification_slot: false,
        log_messages_bytes_limit: None,
        limit_to_load_programs: true,
        recording_config,
        drop_on_failure: false,
        all_or_nothing: false,
    };

    let mut metrics = TransactionErrorMetrics::default();
    let result = bank.load_and_execute_transactions(
        &batch,
        MAX_PROCESSING_AGE,
        &mut timings,
        &mut metrics,
        configs,
    );

    let runtime_transaction_ref = &transactions[0];

    let account_keys = proto_context
        .tx
        .as_ref()
        .and_then(|tx| tx.message.as_ref())
        .map(|message| message.account_keys.clone())
        .unwrap_or_default();

    let mut txn_result = output_txn_result_from_result(result, runtime_transaction_ref.message());
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
