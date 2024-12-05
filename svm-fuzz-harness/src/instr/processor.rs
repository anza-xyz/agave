//! Instruction processor for instruction fixture inputs.

use {
    crate::{
        instr::{context::InstrContext, effects::InstrEffects, program_cache::HarnessProgramCache},
        proto::{InstrContext as ProtoInstrContext, InstrEffects as ProtoInstrEffects},
    },
    solana_account::{Account, AccountSharedData, ReadableAccount},
    solana_clock::Clock,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_epoch_schedule::EpochSchedule,
    solana_instruction::{error::InstructionError, AccountMeta},
    solana_log_collector::LogCollector,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext},
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk::transaction_context::{
        IndexOfAccount, InstructionAccount, TransactionAccount, TransactionContext,
    },
    solana_timings::ExecuteTimings,
    std::sync::Arc,
};

fn setup_sysvar_cache(input_accounts: &[(Pubkey, Account)]) -> SysvarCache {
    let mut sysvar_cache = SysvarCache::default();

    sysvar_cache.fill_missing_entries(|pubkey, callbackback| {
        if let Some(account) = input_accounts.iter().find(|(key, _)| key == pubkey) {
            if account.1.lamports > 0 {
                callbackback(&account.1.data);
            }
        }
    });

    // Any default values for missing sysvar values should be set here
    sysvar_cache.fill_missing_entries(|pubkey, callbackback| {
        if *pubkey == solana_sdk_ids::sysvar::clock::id() {
            // Set the default clock slot to something arbitrary beyond 0
            // This prevents DelayedVisibility errors when executing BPF programs
            callbackback(
                &bincode::serialize(&Clock {
                    slot: 10,
                    ..Default::default()
                })
                .unwrap(),
            );
        }
        if *pubkey == solana_sdk_ids::sysvar::epoch_schedule::id() {
            callbackback(&bincode::serialize(&EpochSchedule::default()).unwrap());
        }
        if *pubkey == solana_sdk_ids::sysvar::rent::id() {
            callbackback(&bincode::serialize(&Rent::default()).unwrap());
        }
        if *pubkey == solana_sdk_ids::sysvar::last_restart_slot::id() {
            let slot_val = 5000_u64;
            callbackback(&bincode::serialize(&slot_val).unwrap());
        }
    });

    sysvar_cache
}

fn setup_instruction_accounts(
    transaction_accounts: &[TransactionAccount],
    account_metas: &[AccountMeta],
) -> Vec<InstructionAccount> {
    let mut instruction_accounts: Vec<InstructionAccount> = Vec::with_capacity(account_metas.len());
    for (instruction_account_index, account_meta) in account_metas.iter().enumerate() {
        let index_in_transaction = transaction_accounts
            .iter()
            .position(|(key, _account)| *key == account_meta.pubkey)
            .unwrap_or(transaction_accounts.len())
            as IndexOfAccount;
        let index_in_callee = instruction_accounts
            .get(0..instruction_account_index)
            .unwrap()
            .iter()
            .position(|instruction_account| {
                instruction_account.index_in_transaction == index_in_transaction
            })
            .unwrap_or(instruction_account_index) as IndexOfAccount;
        instruction_accounts.push(InstructionAccount {
            index_in_transaction,
            index_in_caller: index_in_transaction,
            index_in_callee,
            is_signer: account_meta.is_signer,
            is_writable: account_meta.is_writable,
        });
    }
    instruction_accounts
}

fn execute_instr(mut input: InstrContext) -> Option<InstrEffects> {
    let compute_budget = ComputeBudget {
        compute_unit_limit: input.cu_avail,
        ..ComputeBudget::default()
    };

    let sysvar_cache = setup_sysvar_cache(&input.accounts);

    let clock = sysvar_cache.get_clock().unwrap();
    let epoch_schedule = sysvar_cache.get_epoch_schedule().unwrap();
    let rent = sysvar_cache.get_rent().unwrap();

    // Add checks for rent boundaries
    if rent.lamports_per_byte_year > u32::MAX.into()
        || rent.exemption_threshold > 999.0
        || rent.exemption_threshold < 0.0
        || rent.burn_percent > 100
    {
        return None;
    };

    let transaction_accounts: Vec<TransactionAccount> = input
        .accounts
        .iter()
        .map(|(pubkey, account)| (*pubkey, AccountSharedData::from(account.clone())))
        .collect();

    let program_idx = transaction_accounts
        .iter()
        .position(|(pubkey, _)| *pubkey == input.instruction.program_id)?;

    let mut program_cache = HarnessProgramCache::new(&input.feature_set, &compute_budget);

    // Skip if the program account is a native program and is not owned by the native loader
    // (Would call the owner instead)
    if program_cache
        .builtin_program_ids
        .contains(&transaction_accounts[program_idx].0)
        && transaction_accounts[program_idx].1.owner() != &solana_sdk_ids::native_loader::id()
    {
        return None;
    }

    #[allow(deprecated)]
    let (blockhash, lamports_per_signature) = sysvar_cache
        .get_recent_blockhashes()
        .ok()
        .and_then(|x| (*x).last().cloned())
        .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
        .unwrap_or_default();

    input.last_blockhash = blockhash;
    input.lamports_per_signature = lamports_per_signature;
    input.rent_collector.epoch = clock.epoch;
    input.rent_collector.epoch_schedule = (*epoch_schedule).clone();
    input.rent_collector.rent = (*rent).clone();

    // TODO: Implement duplicate program load check.

    let instruction_accounts =
        setup_instruction_accounts(&transaction_accounts, &input.instruction.accounts);

    let mut transaction_context = TransactionContext::new(
        transaction_accounts,
        (*rent).clone(),
        compute_budget.max_instruction_stack_depth,
        compute_budget.max_instruction_trace_length,
    );

    let env_config = EnvironmentConfig::new(
        blockhash,
        lamports_per_signature,
        0,
        &|_| 0,
        Arc::new(input.feature_set.clone()),
        &sysvar_cache,
    );
    let mut invoke_context = InvokeContext::new(
        &mut transaction_context,
        &mut program_cache.cache,
        env_config,
        Some(LogCollector::new_ref()),
        compute_budget,
    );

    let program_indices = &[program_idx as u16];
    let mut compute_units_consumed = 0u64;
    let mut timings = ExecuteTimings::default();

    // TODO: Handle precompiles.

    let result = invoke_context.process_instruction(
        &input.instruction.data,
        &instruction_accounts,
        program_indices,
        &mut compute_units_consumed,
        &mut timings,
    );

    let custom_err = if let Err(InstructionError::Custom(code)) = result {
        Some(code)
    } else {
        None
    };

    let modified_accounts = input
        .accounts
        .iter()
        .map(|(pubkey, account)| {
            transaction_context
                .find_index_of_account(pubkey)
                .map(|index| {
                    let resulting_account =
                        transaction_context.get_account_at_index(index).unwrap();
                    (*pubkey, resulting_account.take().into())
                })
                .unwrap_or((*pubkey, account.clone()))
        })
        .collect();

    let cu_avail = input.cu_avail.saturating_sub(compute_units_consumed);
    let return_data = transaction_context.get_return_data().1.to_vec();

    Some(InstrEffects {
        result: result.err(),
        custom_err,
        modified_accounts,
        cu_avail,
        return_data,
    })
}

pub fn execute_instr_proto(input: ProtoInstrContext) -> Option<ProtoInstrEffects> {
    let Ok(instr_context) = InstrContext::try_from(input) else {
        return None;
    };
    let instr_effects = execute_instr(instr_context);
    instr_effects.map(Into::into)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::proto::{AcctState as ProtoAcctState, InstrAcct as ProtoInstrAcct},
    };

    #[test]
    fn test_system_program_exec() {
        let native_loader_id = solana_sdk::native_loader::id().to_bytes().to_vec();

        // Ensure that a basic account transfer works
        let input = ProtoInstrContext {
            program_id: vec![0u8; 32],
            accounts: vec![
                ProtoAcctState {
                    address: vec![1u8; 32],
                    owner: vec![0u8; 32],
                    lamports: 1000,
                    data: vec![],
                    executable: false,
                    rent_epoch: 0,
                    seed_addr: None,
                },
                ProtoAcctState {
                    address: vec![2u8; 32],
                    owner: vec![0u8; 32],
                    lamports: 0,
                    data: vec![],
                    executable: false,
                    rent_epoch: 0,
                    seed_addr: None,
                },
                ProtoAcctState {
                    address: vec![0u8; 32],
                    owner: native_loader_id.clone(),
                    lamports: 10000000,
                    data: b"Solana Program".to_vec(),
                    executable: true,
                    rent_epoch: 0,
                    seed_addr: None,
                },
            ],
            instr_accounts: vec![
                ProtoInstrAcct {
                    index: 0,
                    is_signer: true,
                    is_writable: true,
                },
                ProtoInstrAcct {
                    index: 1,
                    is_signer: false,
                    is_writable: true,
                },
            ],
            data: vec![
                // Transfer
                0x02, 0x00, 0x00, 0x00, // Lamports
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            cu_avail: 10000u64,
            epoch_context: None,
            slot_context: None,
        };
        let output = execute_instr_proto(input);
        assert_eq!(
            output,
            Some(ProtoInstrEffects {
                result: 0,
                custom_err: 0,
                modified_accounts: vec![
                    ProtoAcctState {
                        address: vec![1u8; 32],
                        owner: vec![0u8; 32],
                        lamports: 999,
                        data: vec![],
                        executable: false,
                        rent_epoch: 0,
                        seed_addr: None,
                    },
                    ProtoAcctState {
                        address: vec![2u8; 32],
                        owner: vec![0u8; 32],
                        lamports: 1,
                        data: vec![],
                        executable: false,
                        rent_epoch: 0,
                        seed_addr: None,
                    },
                    ProtoAcctState {
                        address: vec![0u8; 32],
                        owner: native_loader_id.clone(),
                        lamports: 10000000,
                        data: b"Solana Program".to_vec(),
                        executable: true,
                        rent_epoch: 0,
                        seed_addr: None,
                    },
                ],
                cu_avail: 9850u64,
                return_data: vec![],
            })
        );
    }
}
