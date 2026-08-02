//! Transaction conformance harness.

use {
    super::{context::TxnContext, effects::TxnEffects},
    crate::{
        account_loader::construct_instructions_account,
        conformance::{
            callback::DefaultCallback,
            nonce_fields::NonceFields,
            setup::{
                InvokeContextFields, compute_budget as default_compute_budget,
                prepare_transaction_invoke_context_fields, program_runtime_environments,
            },
            transaction_meta::TransactionConfiguration,
        },
    },
    solana_account::{Account, AccountSharedData},
    solana_fee_structure::FeeDetails,
    solana_instructions_sysvar::check_id as check_instructions_sysvar_id,
    solana_program_runtime::{
        execution_budget::SVMTransactionExecutionBudget, invoke_context::InvokeContext,
        loaded_programs::ProgramCacheForTxBatch, sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_svm_callback::InvokeContextCallback,
    solana_svm_timings::ExecuteTimings,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_transaction_context::transaction::TransactionContext,
    solana_transaction_error::{TransactionError, TransactionResult},
    std::{collections::HashMap, rc::Rc},
};
#[cfg(feature = "conformance")]
use {
    crate::conformance::{
        callback::ConformanceCallback,
        direct_mapping::direct_mapping_handle_cu_exhaustion,
        programs::{fill_program_cache_from_accounts, new_program_cache_with_builtins},
        setup::sysvar_cache_from_accounts,
    },
    agave_feature_set::virtual_address_space_adjustments,
    agave_precompiles::is_precompile,
    prost::Message,
    protosol::protos::{TxnContext as ProtoTxnContext, TxnResult as ProtoTxnResult},
    solana_instruction::error::InstructionError,
    std::ffi::c_int,
};

pub fn execute_txn(
    input: &TxnContext,
    program_cache: &mut ProgramCacheForTxBatch,
    sysvar_cache: &SysvarCache,
) -> TxnEffects {
    execute_txn_with_callback(input, &DefaultCallback, program_cache, sysvar_cache)
}

pub fn execute_txn_with_callback<C: InvokeContextCallback>(
    input: &TxnContext,
    invoke_callback: &C,
    program_cache: &mut ProgramCacheForTxBatch,
    sysvar_cache: &SysvarCache,
) -> TxnEffects {
    let rent = sysvar_cache
        .get_rent()
        .map(|rent| (*rent).clone())
        .unwrap_or_default();
    let sanitized_message = &input.message;

    let config = match TransactionConfiguration::try_from((sanitized_message, &input.feature_set)) {
        Ok(config) => config,
        Err(err) => return TxnEffects::from_unprocessed_error(err),
    };
    let execution_budget = SVMTransactionExecutionBudget {
        compute_unit_limit: u64::from(config.compute_unit_limit).min(input.cu_avail),
        heap_size: config.updated_heap_bytes,
        ..SVMTransactionExecutionBudget::new_with_defaults(
            input.feature_set.snapshot().raise_cpi_nesting_limit_to_8,
        )
    };

    let transaction_accounts = match sanitized_message
        .account_keys()
        .iter()
        .map(|pubkey| -> TransactionResult<(Pubkey, AccountSharedData)> {
            if check_instructions_sysvar_id(pubkey) {
                return Ok((*pubkey, construct_instructions_account(sanitized_message)?));
            }

            let account = input
                .accounts
                .iter()
                .find(|(key, _)| key == pubkey)
                .map(|(_, account)| AccountSharedData::from(account.clone()))
                .expect("transaction account must be provided");
            Ok((*pubkey, account))
        })
        .collect()
    {
        Ok(accounts) => accounts,
        Err(err) => return TxnEffects::from_unprocessed_error(err),
    };

    let runtime_features = input.feature_set.runtime_features();
    let compute_budget = default_compute_budget(&runtime_features);
    let program_runtime_environments =
        program_runtime_environments(&runtime_features, &compute_budget);

    let transaction_context = TransactionContext::new(
        transaction_accounts,
        rent.clone(),
        execution_budget.max_instruction_stack_depth,
        execution_budget.max_instruction_trace_length,
        sanitized_message.num_instructions(),
    );

    let (blockhash, blockhash_lamports_per_signature) = input
        .nonce_fields
        .as_ref()
        .map(|fields| (fields.blockhash, fields.blockhash_lamports_per_signature))
        .unwrap_or((*input.message.recent_blockhash(), 0));

    let InvokeContextFields {
        sanitized_message,
        mut transaction_context,
        environment_config,
        log_collector,
        execution_budget,
        execution_cost,
    } = prepare_transaction_invoke_context_fields(
        sanitized_message.clone(),
        transaction_context,
        invoke_callback,
        &runtime_features,
        sysvar_cache,
        &compute_budget,
        execution_budget,
        &program_runtime_environments,
        NonceFields {
            blockhash,
            blockhash_lamports_per_signature,
        },
    );
    let mut timings = ExecuteTimings::default();
    let mut executed_units = 0;

    let status = {
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            program_cache,
            environment_config,
            Some(log_collector.clone()),
            execution_budget,
            execution_cost,
        );

        invoke_context
            .process_message(&sanitized_message, &mut timings, &mut executed_units)
            .map_err(|(index, err)| TransactionError::InstructionError(index, err))
    };

    let return_data = transaction_context.get_return_data().1.to_vec();

    let account_keys = (0..transaction_context.get_number_of_accounts())
        .map(|index| {
            *transaction_context
                .get_key_of_account_at_index(index)
                .expect("account index must exist")
        })
        .collect::<Vec<_>>();
    let resulting_account_overrides = transaction_context
        .deconstruct_without_keys()
        .expect("transaction context must be deconstructable")
        .into_iter()
        .zip(account_keys)
        .map(|(account, pubkey)| (pubkey, account))
        .collect::<HashMap<_, _>>();
    let resulting_accounts = input
        .accounts
        .iter()
        .map(|(pubkey, account)| {
            (
                *pubkey,
                resulting_account_overrides
                    .get(pubkey)
                    .cloned()
                    .map(Account::from)
                    .unwrap_or_else(|| account.clone()),
            )
        })
        .collect();

    let logs = Rc::try_unwrap(log_collector)
        .ok()
        .map(|cell| cell.into_inner().into_messages())
        .unwrap_or_default();

    let cu_avail = execution_budget
        .compute_unit_limit
        .saturating_sub(executed_units);

    TxnEffects {
        executed: true,
        status,
        resulting_accounts,
        return_data,
        executed_units,
        fee_details: FeeDetails::new(0, 0),
        loaded_accounts_data_size: 0,
        logs,
        cu_avail,
    }
}

#[cfg(feature = "conformance")]
pub fn execute_txn_proto(input: ProtoTxnContext) -> ProtoTxnResult {
    let epoch_total_stake = input
        .bank
        .as_ref()
        .map(|bank| bank.total_epoch_stake)
        .unwrap_or_default();
    let context = TxnContext::from(input);

    let sysvar_cache = sysvar_cache_from_accounts(&context.accounts);
    let mut program_cache = {
        let slot = sysvar_cache.get_clock().unwrap().slot;
        let runtime_features = context.feature_set.runtime_features();
        let compute_budget = default_compute_budget(&runtime_features);
        let environments = program_runtime_environments(&runtime_features, &compute_budget);

        let accounts = context.accounts.clone();

        let mut cache = new_program_cache_with_builtins(slot);
        fill_program_cache_from_accounts(
            &mut cache,
            environments.get_env_for_execution(),
            &accounts,
            slot,
        );

        cache
    };

    let callback = ConformanceCallback { epoch_total_stake };
    let mut effects =
        execute_txn_with_callback(&context, &callback, &mut program_cache, &sysvar_cache);

    let precompile_custom_error_index = match &effects.status {
        Err(TransactionError::InstructionError(index, InstructionError::Custom(_))) => context
            .message
            .instructions()
            .get(usize::from(*index))
            .and_then(|instruction| {
                context
                    .message
                    .static_account_keys()
                    .get(usize::from(instruction.program_id_index))
            })
            .is_some_and(|program_id| is_precompile(program_id, |_| true))
            .then_some(*index),
        _ => None,
    };

    if let Some(index) = precompile_custom_error_index {
        effects.status = Err(TransactionError::InstructionError(
            index,
            InstructionError::Custom(0),
        ));
    }

    let cu_avail = effects.cu_avail;
    let has_err = effects.status.is_err();
    let mut result: ProtoTxnResult = effects.into();

    direct_mapping_handle_cu_exhaustion(
        context
            .feature_set
            .is_active(&virtual_address_space_adjustments::id()),
        cu_avail,
        has_err,
        result
            .modified_accounts
            .iter_mut()
            .map(|account| &mut account.data),
    );

    result
}

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point
/// to a writable buffer of at least `*out_psz` bytes. On return, `*out_psz`
/// is updated to the number of bytes written.
#[cfg(feature = "conformance")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_svm_txn_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    if in_ptr.is_null() || out_ptr.is_null() || out_psz.is_null() {
        return 0;
    }
    let in_slice = unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) };
    let Ok(txn_context) = ProtoTxnContext::decode(in_slice) else {
        return 0;
    };
    let txn_result = execute_txn_proto(txn_context);
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize) };
    let out_vec = txn_result.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe {
        *out_psz = out_vec.len() as u64;
    }
    1
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "conformance")]
    use protosol::protos::{
        AcctState as ProtoAcctState, CompiledInstruction as ProtoCompiledInstruction,
        MessageHeader as ProtoMessageHeader, SanitizedTransaction as ProtoSanitizedTransaction,
        TransactionMessage as ProtoTransactionMessage,
    };
    use {
        super::*,
        crate::conformance::{
            programs::{keyed_account_for_system_program, new_program_cache_with_builtins},
            setup::{sanitized_message_from_versioned_message, sysvar_cache_from_accounts},
        },
        agave_feature_set::FeatureSet,
        solana_account::ReadableAccount,
        solana_address_lookup_table_interface::state::{AddressLookupTable, LookupTableMeta},
        solana_clock::Clock,
        solana_instruction::error::InstructionError,
        solana_message::{
            AddressLookupTableAccount, Message as LegacyMessage, VersionedMessage, v0,
        },
        solana_sdk_ids::{system_program, sysvar},
        solana_system_interface::instruction::transfer,
        solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as SYSTEM_TRANSFER_CUS,
        std::borrow::Cow,
        test_case::test_case,
    };

    fn with_system_program(mut accounts: Vec<(Pubkey, Account)>) -> Vec<(Pubkey, Account)> {
        accounts.push(keyed_account_for_system_program());
        accounts
    }

    #[test_case(false; "legacy")]
    #[test_case(true; "v0")]
    fn test_execute_message_with_multiple_transfers(is_v0: bool) {
        const FROM_LAMPORTS: u64 = 5_000_000;
        const TO_ONE_LAMPORTS: u64 = 1_000_000;
        const TO_TWO_LAMPORTS: u64 = 2_000_000;
        const AMOUNT_ONE: u64 = 1_000;
        const AMOUNT_TWO: u64 = 2_000;

        let from = Pubkey::new_unique();
        let to_one = Pubkey::new_unique();
        let to_two = Pubkey::new_unique();
        // must be preserved in effects even if the transaction never references it
        let unused = Pubkey::new_unique();
        let blockhash = [0u8; 32].into();
        let instructions = vec![
            transfer(&from, &to_one, AMOUNT_ONE),
            transfer(&from, &to_two, AMOUNT_TWO),
        ];
        let message = if is_v0 {
            let message = v0::Message::try_compile(&from, &instructions, &[], blockhash).unwrap();
            assert_eq!(message.instructions.len(), 2);
            VersionedMessage::V0(message)
        } else {
            let message = LegacyMessage::new_with_blockhash(&instructions, Some(&from), &blockhash);
            assert_eq!(message.instructions.len(), 2);
            VersionedMessage::Legacy(message)
        };

        let accounts = with_system_program(vec![
            (from, Account::new(FROM_LAMPORTS, 0, &system_program::id())),
            (
                to_one,
                Account::new(TO_ONE_LAMPORTS, 0, &system_program::id()),
            ),
            (
                to_two,
                Account::new(TO_TWO_LAMPORTS, 0, &system_program::id()),
            ),
            (unused, Account::new(1, 0, &system_program::id())),
        ]);
        let message = sanitized_message_from_versioned_message(message, &accounts);
        let context =
            TxnContext::new_with_default_budget(FeatureSet::default(), accounts, message, None);

        let sysvar_cache = sysvar_cache_from_accounts(&context.accounts);
        let mut program_cache = new_program_cache_with_builtins(0);

        let effects = execute_txn(&context, &mut program_cache, &sysvar_cache);

        assert!(effects.executed);
        assert_eq!(effects.status, Ok(()));
        assert_eq!(
            effects
                .resulting_accounts
                .iter()
                .map(|(pubkey, _)| *pubkey)
                .collect::<Vec<_>>(),
            vec![from, to_one, to_two, unused, system_program::id()]
        );
        assert_eq!(
            effects.get_account(&from).unwrap().lamports(),
            FROM_LAMPORTS - AMOUNT_ONE - AMOUNT_TWO
        );
        assert_eq!(
            effects.get_account(&to_one).unwrap().lamports(),
            TO_ONE_LAMPORTS + AMOUNT_ONE
        );
        assert_eq!(
            effects.get_account(&to_two).unwrap().lamports(),
            TO_TWO_LAMPORTS + AMOUNT_TWO
        );
    }

    #[test_case(false; "legacy")]
    #[test_case(true; "v0")]
    fn test_resulting_accounts_preserve_input_order(is_v0: bool) {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let c = Pubkey::new_unique();

        let blockhash = [0u8; 32].into();
        let instructions = [transfer(&a, &b, 3)];
        let message = if is_v0 {
            VersionedMessage::V0(
                v0::Message::try_compile(&a, &instructions, &[], blockhash).unwrap(),
            )
        } else {
            VersionedMessage::Legacy(LegacyMessage::new_with_blockhash(
                &instructions,
                Some(&a),
                &blockhash,
            ))
        };

        let accounts = with_system_program(vec![
            (c, Account::new(1, 0, &system_program::id())),
            (b, Account::new(1, 0, &system_program::id())),
            (a, Account::new(1, 0, &system_program::id())),
        ]);
        let message = sanitized_message_from_versioned_message(message, &accounts);
        let context =
            TxnContext::new_with_default_budget(FeatureSet::default(), accounts, message, None);

        let sysvar_cache = sysvar_cache_from_accounts(&context.accounts);
        let mut program_cache = new_program_cache_with_builtins(0);

        let effects = execute_txn(&context, &mut program_cache, &sysvar_cache);

        assert_eq!(
            effects
                .resulting_accounts
                .iter()
                .map(|(pubkey, _)| *pubkey)
                .collect::<Vec<_>>(),
            vec![c, b, a, system_program::id()]
        );
    }

    #[test]
    fn test_execute_message_with_lut() {
        let payer = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        let lookup_table_key = Pubkey::new_unique();
        let clock_pubkey = sysvar::clock::id();
        let blockhash = [0u8; 32].into();
        let instructions = [transfer(&payer, &to, 3)];
        let lookup_addresses = vec![
            Pubkey::new_unique(),
            to,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let lookup_table_account = AddressLookupTable {
            meta: LookupTableMeta::default(),
            addresses: Cow::Owned(lookup_addresses.clone()),
        };
        let lookup_table = AddressLookupTableAccount {
            key: lookup_table_key,
            addresses: lookup_addresses.clone(),
        };
        let message =
            v0::Message::try_compile(&payer, &instructions, &[lookup_table], blockhash).unwrap();

        let accounts = with_system_program(vec![
            (to, Account::new(1, 0, &system_program::id())),
            (
                lookup_table_key,
                Account {
                    lamports: 1,
                    data: lookup_table_account.serialize_for_tests().unwrap(),
                    owner: solana_address_lookup_table_interface::program::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            ),
            (payer, Account::new(5_000_000, 0, &system_program::id())),
            (
                clock_pubkey,
                Account {
                    lamports: 1,
                    data: bincode::serialize(&Clock {
                        slot: 1,
                        ..Clock::default()
                    })
                    .unwrap(),
                    owner: sysvar::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            ),
        ]);
        let message =
            sanitized_message_from_versioned_message(VersionedMessage::V0(message), &accounts);
        let context =
            TxnContext::new_with_default_budget(FeatureSet::default(), accounts, message, None);
        let sysvar_cache = sysvar_cache_from_accounts(&context.accounts);
        let mut program_cache = new_program_cache_with_builtins(1);

        let effects = execute_txn(&context, &mut program_cache, &sysvar_cache);

        assert_eq!(effects.status, Ok(()));
        assert_eq!(effects.get_account(&to).unwrap().lamports(), 4);
        assert_eq!(
            effects
                .resulting_accounts
                .iter()
                .map(|(pubkey, _)| *pubkey)
                .collect::<Vec<_>>(),
            vec![
                to,
                lookup_table_key,
                payer,
                clock_pubkey,
                system_program::id()
            ]
        );
    }

    #[cfg(feature = "conformance")]
    #[test]
    fn test_execute_txn_proto() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let c = Pubkey::new_unique();

        let clock_pubkey = sysvar::clock::id();
        let blockhash = [0u8; 32];
        let instruction = transfer(&a, &b, 1);
        let (system_program_id, system_program_account) = keyed_account_for_system_program();

        let result = execute_txn_proto(ProtoTxnContext {
            tx: Some(ProtoSanitizedTransaction {
                message: Some(ProtoTransactionMessage {
                    is_legacy: true,
                    header: Some(ProtoMessageHeader {
                        num_required_signatures: 1,
                        num_readonly_signed_accounts: 0,
                        num_readonly_unsigned_accounts: 1,
                    }),
                    account_keys: vec![
                        a.to_bytes().to_vec(),
                        b.to_bytes().to_vec(),
                        system_program::id().to_bytes().to_vec(),
                    ],
                    recent_blockhash: blockhash.to_vec(),
                    instructions: vec![ProtoCompiledInstruction {
                        program_id_index: 2,
                        accounts: vec![0, 1],
                        data: instruction.data,
                    }],
                    address_table_lookups: vec![],
                }),
                message_hash: vec![],
                signatures: vec![],
            }),
            account_shared_data: vec![
                ProtoAcctState {
                    address: c.to_bytes().to_vec(),
                    lamports: 1,
                    data: vec![],
                    executable: false,
                    owner: system_program::id().to_bytes().to_vec(),
                },
                ProtoAcctState {
                    address: b.to_bytes().to_vec(),
                    lamports: 1,
                    data: vec![],
                    executable: false,
                    owner: system_program::id().to_bytes().to_vec(),
                },
                ProtoAcctState {
                    address: a.to_bytes().to_vec(),
                    lamports: 1,
                    data: vec![],
                    executable: false,
                    owner: system_program::id().to_bytes().to_vec(),
                },
                ProtoAcctState {
                    address: clock_pubkey.to_bytes().to_vec(),
                    lamports: 1,
                    data: bincode::serialize(&Clock {
                        slot: 1,
                        ..Clock::default()
                    })
                    .unwrap(),
                    executable: false,
                    owner: sysvar::id().to_bytes().to_vec(),
                },
                ProtoAcctState {
                    address: system_program_id.to_bytes().to_vec(),
                    lamports: system_program_account.lamports,
                    data: system_program_account.data,
                    executable: system_program_account.executable,
                    owner: system_program_account.owner.to_bytes().to_vec(),
                },
            ],
            bank: None,
        });

        assert!(result.executed);
        assert_eq!(
            result
                .modified_accounts
                .iter()
                .map(|account| Pubkey::try_from(account.address.as_slice()).unwrap())
                .collect::<Vec<_>>(),
            vec![c, b, a, clock_pubkey, system_program_id]
        );
    }

    #[test]
    fn test_execute_txn_cu_avail_choke_execution() {
        let from = Pubkey::new_unique();
        let to = Pubkey::new_unique();

        let blockhash = [0u8; 32].into();
        let instruction = transfer(&from, &to, 1);
        let message = VersionedMessage::Legacy(LegacyMessage::new_with_blockhash(
            &[instruction],
            Some(&from),
            &blockhash,
        ));

        let accounts = with_system_program(vec![
            (from, Account::new(5_000_000, 0, &system_program::id())),
            (to, Account::new(1, 0, &system_program::id())),
        ]);
        let message = sanitized_message_from_versioned_message(message, &accounts);
        let context = TxnContext {
            feature_set: FeatureSet::default(),
            accounts,
            message,
            nonce_fields: None,
            cu_avail: SYSTEM_TRANSFER_CUS - 1,
        };
        let sysvar_cache = sysvar_cache_from_accounts(&context.accounts);
        let mut program_cache = new_program_cache_with_builtins(0);

        let effects = execute_txn(&context, &mut program_cache, &sysvar_cache);

        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                0,
                InstructionError::ComputationalBudgetExceeded
            ))
        );
        assert_eq!(effects.cu_avail, 0);
    }
}
