//! Solana SVM fuzz harness for instruction.
//!
//! This module represents the entrypoint function available for fuzzing or
//! conformance testing, and includes various setup that is not imposed on the
//! developer in the base entrypoint.
//!
//! Specifically, this harness is just one implementation wherein various
//! required components - such as the program cache and sysvar cache - are
//! configured, various checks are imposed, and the base entrypoint for the
//! program runtime is invoked (`solana-svm-fuzz-harness-instr-entrypoint`).
//!
//! This particular harness is used by the Firedancer team to test Firedancer's
//! conformance with Agave. Other validator clients may wish to use this same
//! harness, or define their own, which can be built on the base entrypoint in
//! similar fashion.

mod program_cache;
mod sysvar_cache;

use {
    prost::Message,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_instruction::error::InstructionError,
    solana_message::compiled_instruction::CompiledInstruction,
    solana_precompiles::{is_precompile, verify_if_precompile},
    solana_pubkey::Pubkey,
    solana_svm::transaction_processing_callback::TransactionProcessingCallback,
    solana_svm_fuzz_harness_fixture::{
        invoke::{context::InstrContext, effects::InstrEffects},
        proto::{InstrContext as ProtoInstrContext, InstrEffects as ProtoInstrEffects},
    },
    solana_svm_fuzz_harness_instr_entrypoint::{
        process_instruction, EnvironmentContext, InstructionResult,
    },
    solana_timings::ExecuteTimings,
    solana_transaction_context::{IndexOfAccount, InstructionAccount},
    std::{collections::HashSet, ffi::c_int},
};

/// Implement the callback trait so that the SVM API can be used to load
/// program ELFs from accounts (ie. `load_program_with_pubkey`).
struct InstrContextCallback<'a>(&'a InstrContext);

impl TransactionProcessingCallback for InstrContextCallback<'_> {
    fn account_matches_owners(&self, address: &Pubkey, owners: &[Pubkey]) -> Option<usize> {
        self.0
            .accounts
            .iter()
            .find(|(pubkey, _)| pubkey == address)
            .and_then(|(_, acct)| owners.iter().position(|o| acct.owner() == o))
    }

    fn get_account_shared_data(&self, address: &Pubkey) -> Option<AccountSharedData> {
        self.0
            .accounts
            .iter()
            .find(|(pubkey, _)| pubkey == address)
            .map(|(_, acct)| acct.clone())
    }
}

fn execute_instr(input: InstrContext) -> Option<InstrEffects> {
    let compute_budget = ComputeBudget {
        compute_unit_limit: input.compute_units_available,
        ..ComputeBudget::default()
    };

    // Set up the sysvar cache.
    let sysvar_cache = crate::sysvar_cache::setup_sysvar_cache(&input.accounts);

    #[allow(deprecated)]
    let (blockhash, lamports_per_signature) = sysvar_cache
        .get_recent_blockhashes()
        .ok()
        .and_then(|x| (*x).last().cloned())
        .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
        .unwrap_or_default();
    let clock = sysvar_cache.get_clock().unwrap();
    let rent = sysvar_cache.get_rent().unwrap();

    // Add checks for rent boundaries.
    if rent.lamports_per_byte_year > u32::MAX.into()
        || rent.exemption_threshold > 999.0
        || rent.exemption_threshold < 0.0
        || rent.burn_percent > 100
    {
        return None;
    };

    // Set up the program cache, which will include all builtins by default.
    let mut program_cache = crate::program_cache::setup_program_cache(
        &input.epoch_context.feature_set,
        &compute_budget,
        clock.slot,
    );

    // Check for duplicate account loads.
    let mut loads = HashSet::<Pubkey>::new();

    // Iterate over the provided accounts to check for duplicate loads as well
    // as load any provided BPF programs into the cache.
    for (pubkey, account) in &input.accounts {
        if *pubkey == input.program_id {
            continue;
        }

        // FD rejects duplicate account loads.
        if !loads.insert(*pubkey) {
            return None;
        }

        if account.executable() && program_cache.find(pubkey).is_none() {
            // SVM API's `load_program_with_pubkey`` expects the owner to be
            // one of the BPF loaders.
            if !solana_sdk_ids::loader_v4::check_id(account.owner())
                && !solana_sdk_ids::bpf_loader_deprecated::check_id(account.owner())
                && !solana_sdk_ids::bpf_loader::check_id(account.owner())
                && !solana_sdk_ids::bpf_loader_upgradeable::check_id(account.owner())
            {
                continue;
            }

            // Load the program into the cache using the SVM API.
            if let Some(loaded_program) = solana_svm::program_loader::load_program_with_pubkey(
                &InstrContextCallback(&input),
                &program_cache.environments,
                pubkey,
                clock.slot,
                &mut ExecuteTimings::default(),
                false,
            ) {
                program_cache.replenish(*pubkey, loaded_program);
            }
        }
    }

    // Build out the list of `InstructionAccount`.
    let instruction_accounts = {
        let mut instruction_accounts: Vec<InstructionAccount> =
            Vec::with_capacity(input.instruction_accounts.len());

        for (instruction_account_index, instruction_account) in
            input.instruction_accounts.iter().enumerate()
        {
            let index_in_transaction = instruction_account.index;

            let index_in_callee = input
                .instruction_accounts
                .get(0..instruction_account_index)
                .unwrap()
                .iter()
                .position(|instruction_account| instruction_account.index == index_in_transaction)
                .unwrap_or(instruction_account_index)
                as IndexOfAccount;

            instruction_accounts.push(InstructionAccount {
                index_in_transaction: index_in_transaction as IndexOfAccount,
                index_in_caller: index_in_transaction as IndexOfAccount,
                index_in_callee,
                is_signer: instruction_account.is_signer,
                is_writable: instruction_account.is_writable,
            });
        }

        instruction_accounts
    };

    // Precompiles (ed25519, secp256k1)
    // Precompiles are programs that run without the VM and without loading any account.
    // They allow to verify signatures, either ed25519 or Ethereum-like secp256k1
    // (note that precompiles can access data from other instructions as well, within
    // the same transaction).
    //
    // They're not run as part of transaction execution, but instead they're run during
    // transaction verification:
    // https://github.com/anza-xyz/agave/blob/34b76ac/runtime/src/bank.rs#L5779
    //
    // During transaction execution, they're skipped (just accounted for CU)
    // https://github.com/anza-xyz/agave/blob/34b76ac/svm/src/message_processor.rs#L93-L108
    //
    // Here we're testing a single instruction.
    // Therefore, when the program is a precompile, we need to run the precompile
    // instead of the regular process_instruction().
    // https://github.com/anza-xyz/agave/blob/34b76ac/sdk/src/precompiles.rs#L107
    //
    // Note: while this test covers the functionality of the precompile, it doesn't
    // cover the fact that the precompile can access data from other instructions.
    // This will be covered in separated tests.
    if is_precompile(&input.program_id, |id| {
        input.epoch_context.feature_set.is_active(id)
    }) {
        let compiled_instruction = CompiledInstruction {
            program_id_index: 0,
            accounts: vec![],
            data: input.instruction_data,
        };
        let result = verify_if_precompile(
            &input.program_id,
            &compiled_instruction,
            &[compiled_instruction.clone()],
            &input.epoch_context.feature_set,
        );
        return Some(InstrEffects {
            program_custom_code: None,
            program_result: if result.is_err() {
                // Precompiles return PrecompileError instead of InstructionError, and
                // there's no from/into conversion to InstructionError nor to u32.
                // For simplicity, we remap first-first, second-second, etc.
                Some(InstructionError::GenericError)
            } else {
                None
            },
            modified_accounts: vec![],
            compute_units_available: input.compute_units_available,
            return_data: vec![],
        });
    }

    // Invoke the base entrypoint for program-runtime.
    let InstructionResult {
        compute_units_consumed,
        program_result,
        return_data,
        resulting_accounts,
        ..
    } = process_instruction(
        &input.program_id,
        &instruction_accounts,
        &input.instruction_data,
        &input.accounts,
        &mut program_cache,
        EnvironmentContext::new(
            &blockhash,
            &compute_budget,
            &input.epoch_context.feature_set,
            lamports_per_signature,
            &rent,
            &sysvar_cache,
        ),
    );

    Some(InstrEffects {
        program_custom_code: if let Err(InstructionError::Custom(code)) = program_result {
            Some(code)
        } else {
            None
        },
        program_result: program_result.err(),
        modified_accounts: resulting_accounts,
        compute_units_available: input
            .compute_units_available
            .saturating_sub(compute_units_consumed),
        return_data,
    })
}

/// Main harness for executing Agave's execution-layer instruction entrypoint
/// using a Protobuf instruction context.
///
/// Returns the instruction's effects as a Protobuf instruction "effects".
pub fn execute_instr_proto(input: ProtoInstrContext) -> Option<ProtoInstrEffects> {
    let instr_context = InstrContext::try_from(input).unwrap();
    let instr_effects = execute_instr(instr_context);
    instr_effects.map(Into::into)
}

/// # Safety
#[no_mangle]
pub unsafe extern "C" fn sol_compat_instr_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    let in_slice = std::slice::from_raw_parts(in_ptr, in_sz as usize);
    let Ok(instr_context) = ProtoInstrContext::decode(in_slice) else {
        return 0;
    };
    let Some(instr_effects) = execute_instr_proto(instr_context) else {
        return 0;
    };
    let out_slice = std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize);
    let out_vec = instr_effects.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    *out_psz = out_vec.len() as u64;

    1
}
