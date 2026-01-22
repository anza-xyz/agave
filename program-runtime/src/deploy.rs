//! Program deployment functionality.

#[cfg(feature = "metrics")]
use {crate::loaded_programs::LoadProgramMetrics, solana_svm_measure::measure::Measure};
use {
    crate::{
        invoke_context::InvokeContext,
        loaded_programs::{
            DELAY_VISIBILITY_SLOT_OFFSET, ProgramCacheEntry, ProgramCacheForTxBatch,
            ProgramRuntimeEnvironment,
        },
    },
    solana_clock::Slot,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_sbpf::{
        elf::{ElfError, Executable},
        program::BuiltinProgram,
        verifier::RequisiteVerifier,
    },
    solana_svm_log_collector::{LogCollector, ic_logger_msg},
    solana_svm_type_overrides::sync::{Arc, atomic::Ordering},
    std::{cell::RefCell, rc::Rc},
};

fn morph_into_deployment_environment_v1<'a>(
    from: Arc<BuiltinProgram<InvokeContext<'a, 'a>>>,
) -> Result<BuiltinProgram<InvokeContext<'a, 'a>>, ElfError> {
    let mut config = from.get_config().clone();
    config.reject_broken_elfs = true;
    // Once the tests are being build using a toolchain which supports the newer SBPF versions,
    // the deployment of older versions will be disabled:
    // config.enabled_sbpf_versions =
    //     *config.enabled_sbpf_versions.end()..=*config.enabled_sbpf_versions.end();

    let mut result = BuiltinProgram::new_loader(config);

    for (_key, (name, value)) in from.get_function_registry().iter() {
        // Deployment of programs with sol_alloc_free is disabled. So do not register the syscall.
        if name != *b"sol_alloc_free_" {
            result.register_function(unsafe { std::str::from_utf8_unchecked(name) }, value)?;
        }
    }

    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub fn load_program_from_bytes(
    log_collector: Option<Rc<RefCell<LogCollector>>>,
    #[cfg(feature = "metrics")] load_program_metrics: &mut LoadProgramMetrics,
    programdata: &[u8],
    loader_key: &Pubkey,
    account_size: usize,
    deployment_slot: Slot,
    program_runtime_environment: Arc<BuiltinProgram<InvokeContext<'static, 'static>>>,
    reloading: bool,
) -> Result<ProgramCacheEntry, InstructionError> {
    let effective_slot = deployment_slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET);
    let loaded_program = if reloading {
        // Safety: this is safe because the program is being reloaded in the cache.
        unsafe {
            ProgramCacheEntry::reload(
                loader_key,
                program_runtime_environment,
                deployment_slot,
                effective_slot,
                programdata,
                account_size,
                #[cfg(feature = "metrics")]
                load_program_metrics,
            )
        }
    } else {
        ProgramCacheEntry::new(
            loader_key,
            program_runtime_environment,
            deployment_slot,
            effective_slot,
            programdata,
            account_size,
            #[cfg(feature = "metrics")]
            load_program_metrics,
        )
    }
    .map_err(|err| {
        ic_logger_msg!(log_collector, "{}", err);
        InstructionError::InvalidAccountData
    })?;
    Ok(loaded_program)
}

/// Directly deploy a program using a provided invoke context.
/// This function should only be invoked from the runtime, since it does not
/// provide any account loads or checks.
#[allow(clippy::too_many_arguments)]
pub fn deploy_program(
    log_collector: Option<Rc<RefCell<LogCollector>>>,
    #[cfg(feature = "metrics")] load_program_metrics: &mut LoadProgramMetrics,
    program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
    program_runtime_environment: ProgramRuntimeEnvironment,
    program_id: &Pubkey,
    loader_key: &Pubkey,
    account_size: usize,
    programdata: &[u8],
    deployment_slot: Slot,
) -> Result<(), InstructionError> {
    #[cfg(feature = "metrics")]
    let mut register_syscalls_time = Measure::start("register_syscalls_time");
    let deployment_program_runtime_environment =
        morph_into_deployment_environment_v1(program_runtime_environment.clone()).map_err(|e| {
            ic_logger_msg!(log_collector, "Failed to register syscalls: {}", e);
            InstructionError::ProgramEnvironmentSetupFailure
        })?;
    #[cfg(feature = "metrics")]
    {
        register_syscalls_time.stop();
        load_program_metrics.register_syscalls_us = register_syscalls_time.as_us();
    }
    // Verify using stricter deployment_program_runtime_environment
    #[cfg(feature = "metrics")]
    let mut load_elf_time = Measure::start("load_elf_time");
    let executable = Executable::<InvokeContext>::load(
        programdata,
        Arc::new(deployment_program_runtime_environment),
    )
    .map_err(|err| {
        ic_logger_msg!(log_collector, "{}", err);
        InstructionError::InvalidAccountData
    })?;
    #[cfg(feature = "metrics")]
    {
        load_elf_time.stop();
        load_program_metrics.load_elf_us = load_elf_time.as_us();
    }
    #[cfg(feature = "metrics")]
    let mut verify_code_time = Measure::start("verify_code_time");
    executable.verify::<RequisiteVerifier>().map_err(|err| {
        ic_logger_msg!(log_collector, "{}", err);
        InstructionError::InvalidAccountData
    })?;
    #[cfg(feature = "metrics")]
    {
        verify_code_time.stop();
        load_program_metrics.verify_code_us = verify_code_time.as_us();
    }
    // Reload but with program_runtime_environment
    let executor = load_program_from_bytes(
        log_collector,
        #[cfg(feature = "metrics")]
        load_program_metrics,
        programdata,
        loader_key,
        account_size,
        deployment_slot,
        program_runtime_environment,
        true,
    )?;
    if let Some(old_entry) = program_cache_for_tx_batch.find(program_id) {
        executor.tx_usage_counter.store(
            old_entry.tx_usage_counter.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }
    #[cfg(feature = "metrics")]
    {
        load_program_metrics.program_id = program_id.to_string();
    }
    program_cache_for_tx_batch.store_modified_entry(*program_id, Arc::new(executor));
    Ok(())
}

#[macro_export]
macro_rules! deploy_program {
    ($invoke_context:expr, $program_id:expr, $loader_key:expr, $account_size:expr, $programdata:expr, $deployment_slot:expr $(,)?) => {
        assert_eq!(
            $deployment_slot,
            $invoke_context.program_cache_for_tx_batch.slot()
        );
        #[cfg(feature = "metrics")]
        let mut load_program_metrics = $crate::loaded_programs::LoadProgramMetrics::default();
        $crate::deploy::deploy_program(
            $invoke_context.get_log_collector(),
            #[cfg(feature = "metrics")]
            &mut load_program_metrics,
            $invoke_context.program_cache_for_tx_batch,
            $invoke_context
                .get_program_runtime_environments_for_deployment()
                .program_runtime_v1
                .clone(),
            $program_id,
            $loader_key,
            $account_size,
            $programdata,
            $deployment_slot,
        )?;
        #[cfg(feature = "metrics")]
        load_program_metrics.submit_datapoint(&mut $invoke_context.timings);
    };
}
