use {
    crate::{
        execution_budget::{SVMTransactionExecutionBudget, SVMTransactionExecutionCost},
        loaded_programs::{
            ProgramCacheEntry, ProgramCacheEntryType, ProgramCacheForTxBatch,
            ProgramRuntimeEnvironments,
        },
        stable_log,
        sysvar_cache::SysvarCache,
    },
    solana_account::{create_account_shared_data_for_test, AccountSharedData},
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_instruction::{error::InstructionError, AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    solana_sbpf::{
        ebpf::MM_HEAP_START,
        elf::Executable as GenericExecutable,
        error::{EbpfError, ProgramResult},
        memory_region::MemoryMapping,
        program::{BuiltinFunction, SBPFVersion},
        vm::{Config, ContextObject, EbpfVm},
    },
    solana_sdk_ids::{
        bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4, native_loader, sysvar,
    },
    solana_svm_callback::InvokeContextCallback,
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_log_collector::{ic_msg, LogCollector},
    solana_svm_measure::measure::Measure,
    solana_svm_timings::{ExecuteDetailsTimings, ExecuteTimings},
    solana_svm_transaction::{instruction::SVMInstruction, svm_message::SVMMessage},
    solana_svm_type_overrides::sync::Arc,
    solana_transaction_context::{
        transaction_accounts::KeyedAccountSharedData, IndexOfAccount, InstructionAccount,
        InstructionContext, TransactionContext, MAX_ACCOUNTS_PER_TRANSACTION,
    },
    std::{
        alloc::Layout,
        borrow::Cow,
        cell::RefCell,
        fmt::{self, Debug},
        rc::Rc,
    },
};

pub type BuiltinFunctionWithContext = BuiltinFunction<InvokeContext<'static, 'static>>;
pub type Executable = GenericExecutable<InvokeContext<'static, 'static>>;
pub type RegisterTrace<'a> = &'a [[u64; 12]];

/// Adapter so we can unify the interfaces of built-in programs and syscalls
#[macro_export]
macro_rules! declare_process_instruction {
    ($process_instruction:ident, $cu_to_consume:expr, |$invoke_context:ident| $inner:tt) => {
        $crate::solana_sbpf::declare_builtin_function!(
            $process_instruction,
            fn rust(
                invoke_context: &mut $crate::invoke_context::InvokeContext,
                _arg0: u64,
                _arg1: u64,
                _arg2: u64,
                _arg3: u64,
                _arg4: u64,
                _memory_mapping: &mut $crate::solana_sbpf::memory_region::MemoryMapping,
            ) -> std::result::Result<u64, Box<dyn std::error::Error>> {
                fn process_instruction_inner(
                    $invoke_context: &mut $crate::invoke_context::InvokeContext,
                ) -> std::result::Result<(), $crate::__private::InstructionError>
                    $inner

                let consumption_result = if $cu_to_consume > 0
                {
                    invoke_context.consume_checked($cu_to_consume)
                } else {
                    Ok(())
                };
                consumption_result
                    .and_then(|_| {
                        process_instruction_inner(invoke_context)
                            .map(|_| 0)
                            .map_err(|err| Box::new(err) as Box<dyn std::error::Error>)
                    })
                    .into()
            }
        );
    };
}

impl ContextObject for InvokeContext<'_, '_> {
    fn consume(&mut self, amount: u64) {
        // 1 to 1 instruction to compute unit mapping
        // ignore overflow, Ebpf will bail if exceeded
        let mut compute_meter = self.compute_meter.borrow_mut();
        *compute_meter = compute_meter.saturating_sub(amount);
    }

    fn get_remaining(&self) -> u64 {
        *self.compute_meter.borrow()
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AllocErr;
impl fmt::Display for AllocErr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("Error: Memory allocation failed")
    }
}

pub struct BpfAllocator {
    len: u64,
    pos: u64,
}

impl BpfAllocator {
    pub fn new(len: u64) -> Self {
        Self { len, pos: 0 }
    }

    pub fn alloc(&mut self, layout: Layout) -> Result<u64, AllocErr> {
        let bytes_to_align = (self.pos as *const u8).align_offset(layout.align()) as u64;
        if self
            .pos
            .saturating_add(bytes_to_align)
            .saturating_add(layout.size() as u64)
            <= self.len
        {
            self.pos = self.pos.saturating_add(bytes_to_align);
            let addr = MM_HEAP_START.saturating_add(self.pos);
            self.pos = self.pos.saturating_add(layout.size() as u64);
            Ok(addr)
        } else {
            Err(AllocErr)
        }
    }
}

pub struct EnvironmentConfig<'a> {
    pub blockhash: Hash,
    pub blockhash_lamports_per_signature: u64,
    epoch_stake_callback: &'a dyn InvokeContextCallback,
    feature_set: &'a SVMFeatureSet,
    pub program_runtime_environments_for_execution: &'a ProgramRuntimeEnvironments,
    pub program_runtime_environments_for_deployment: &'a ProgramRuntimeEnvironments,
    sysvar_cache: &'a SysvarCache,
}
impl<'a> EnvironmentConfig<'a> {
    pub fn new(
        blockhash: Hash,
        blockhash_lamports_per_signature: u64,
        epoch_stake_callback: &'a dyn InvokeContextCallback,
        feature_set: &'a SVMFeatureSet,
        program_runtime_environments_for_execution: &'a ProgramRuntimeEnvironments,
        program_runtime_environments_for_deployment: &'a ProgramRuntimeEnvironments,
        sysvar_cache: &'a SysvarCache,
    ) -> Self {
        Self {
            blockhash,
            blockhash_lamports_per_signature,
            epoch_stake_callback,
            feature_set,
            program_runtime_environments_for_execution,
            program_runtime_environments_for_deployment,
            sysvar_cache,
        }
    }
}

pub struct SyscallContext {
    pub allocator: BpfAllocator,
    pub accounts_metadata: Vec<SerializedAccountMetadata>,
}

#[derive(Debug, Clone)]
pub struct SerializedAccountMetadata {
    pub original_data_len: usize,
    pub vm_data_addr: u64,
    pub vm_key_addr: u64,
    pub vm_lamports_addr: u64,
    pub vm_owner_addr: u64,
}

/// Main pipeline from runtime to program execution.
pub struct InvokeContext<'a, 'ix_data> {
    /// Information about the currently executing transaction.
    pub transaction_context: &'a mut TransactionContext<'ix_data>,
    /// The local program cache for the transaction batch.
    pub program_cache_for_tx_batch: &'a mut ProgramCacheForTxBatch,
    /// Runtime configurations used to provision the invocation environment.
    pub environment_config: EnvironmentConfig<'a>,
    /// The compute budget for the current invocation.
    compute_budget: SVMTransactionExecutionBudget,
    /// The compute cost for the current invocation.
    execution_cost: SVMTransactionExecutionCost,
    /// Instruction compute meter, for tracking compute units consumed against
    /// the designated compute budget during program execution.
    compute_meter: RefCell<u64>,
    log_collector: Option<Rc<RefCell<LogCollector>>>,
    /// Latest measurement not yet accumulated in [ExecuteDetailsTimings::execute_us]
    pub execute_time: Option<Measure>,
    pub timings: ExecuteDetailsTimings,
    pub syscall_context: Vec<Option<SyscallContext>>,
    /// Pairs of index in TX instruction trace and VM register trace
    register_traces: Vec<(usize, Vec<[u64; 12]>)>,
}

impl<'a, 'ix_data> InvokeContext<'a, 'ix_data> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transaction_context: &'a mut TransactionContext<'ix_data>,
        program_cache_for_tx_batch: &'a mut ProgramCacheForTxBatch,
        environment_config: EnvironmentConfig<'a>,
        log_collector: Option<Rc<RefCell<LogCollector>>>,
        compute_budget: SVMTransactionExecutionBudget,
        execution_cost: SVMTransactionExecutionCost,
    ) -> Self {
        Self {
            transaction_context,
            program_cache_for_tx_batch,
            environment_config,
            log_collector,
            compute_budget,
            execution_cost,
            compute_meter: RefCell::new(compute_budget.compute_unit_limit),
            execute_time: None,
            timings: ExecuteDetailsTimings::default(),
            syscall_context: Vec::new(),
            register_traces: Vec::new(),
        }
    }

    /// Push a stack frame onto the invocation stack
    pub fn push(&mut self) -> Result<(), InstructionError> {
        let instruction_context = self
            .transaction_context
            .get_instruction_context_at_index_in_trace(
                self.transaction_context.get_instruction_trace_length(),
            )?;
        let program_id = instruction_context
            .get_program_key()
            .map_err(|_| InstructionError::UnsupportedProgramId)?;
        if self.transaction_context.get_instruction_stack_height() != 0 {
            let contains =
                (0..self.transaction_context.get_instruction_stack_height()).any(|level| {
                    self.transaction_context
                        .get_instruction_context_at_nesting_level(level)
                        .and_then(|instruction_context| instruction_context.get_program_key())
                        .map(|program_key| program_key == program_id)
                        .unwrap_or(false)
                });
            let is_last = self
                .transaction_context
                .get_current_instruction_context()
                .and_then(|instruction_context| instruction_context.get_program_key())
                .map(|program_key| program_key == program_id)
                .unwrap_or(false);
            if contains && !is_last {
                // Reentrancy not allowed unless caller is calling itself
                return Err(InstructionError::ReentrancyNotAllowed);
            }
        }

        self.syscall_context.push(None);
        self.transaction_context.push()
    }

    /// Pop a stack frame from the invocation stack
    fn pop(&mut self) -> Result<(), InstructionError> {
        self.syscall_context.pop();
        self.transaction_context.pop()
    }

    /// Current height of the invocation stack, top level instructions are height
    /// `solana_instruction::TRANSACTION_LEVEL_STACK_HEIGHT`
    pub fn get_stack_height(&self) -> usize {
        self.transaction_context.get_instruction_stack_height()
    }

    /// Entrypoint for a cross-program invocation from a builtin program
    pub fn native_invoke(
        &mut self,
        instruction: Instruction,
        signers: &[Pubkey],
    ) -> Result<(), InstructionError> {
        self.prepare_next_instruction(instruction, signers)?;
        let mut compute_units_consumed = 0;
        self.process_instruction(&mut compute_units_consumed, &mut ExecuteTimings::default())?;
        Ok(())
    }

    /// Helper to prepare for process_instruction() when the instruction is not a top level one,
    /// and depends on `AccountMeta`s
    pub fn prepare_next_instruction(
        &mut self,
        instruction: Instruction,
        signers: &[Pubkey],
    ) -> Result<(), InstructionError> {
        // We reference accounts by an u8 index, so we have a total of 256 accounts.
        let mut transaction_callee_map: Vec<u16> = vec![u16::MAX; MAX_ACCOUNTS_PER_TRANSACTION];
        let mut instruction_accounts: Vec<InstructionAccount> =
            Vec::with_capacity(instruction.accounts.len());

        // This code block is necessary to restrict the scope of the immutable borrow of
        // transaction context (the `instruction_context` variable). At the end of this
        // function, we must borrow it again as mutable.
        let program_account_index = {
            let instruction_context = self.transaction_context.get_current_instruction_context()?;

            for account_meta in instruction.accounts.iter() {
                let index_in_transaction = self
                    .transaction_context
                    .find_index_of_account(&account_meta.pubkey)
                    .ok_or_else(|| {
                        ic_msg!(
                            self,
                            "Instruction references an unknown account {}",
                            account_meta.pubkey,
                        );
                        InstructionError::MissingAccount
                    })?;

                debug_assert!((index_in_transaction as usize) < transaction_callee_map.len());
                let index_in_callee = transaction_callee_map
                    .get_mut(index_in_transaction as usize)
                    .unwrap();

                if (*index_in_callee as usize) < instruction_accounts.len() {
                    let cloned_account = {
                        let instruction_account = instruction_accounts
                            .get_mut(*index_in_callee as usize)
                            .ok_or(InstructionError::MissingAccount)?;
                        instruction_account.set_is_signer(
                            instruction_account.is_signer() || account_meta.is_signer,
                        );
                        instruction_account.set_is_writable(
                            instruction_account.is_writable() || account_meta.is_writable,
                        );
                        *instruction_account
                    };
                    instruction_accounts.push(cloned_account);
                } else {
                    *index_in_callee = instruction_accounts.len() as u16;
                    instruction_accounts.push(InstructionAccount::new(
                        index_in_transaction,
                        account_meta.is_signer,
                        account_meta.is_writable,
                    ));
                }
            }

            for current_index in 0..instruction_accounts.len() {
                let instruction_account = instruction_accounts.get(current_index).unwrap();
                let index_in_callee = *transaction_callee_map
                    .get(instruction_account.index_in_transaction as usize)
                    .unwrap() as usize;

                if current_index != index_in_callee {
                    let (is_signer, is_writable) = {
                        let reference_account = instruction_accounts
                            .get(index_in_callee)
                            .ok_or(InstructionError::MissingAccount)?;
                        (
                            reference_account.is_signer(),
                            reference_account.is_writable(),
                        )
                    };

                    let current_account = instruction_accounts.get_mut(current_index).unwrap();
                    current_account.set_is_signer(current_account.is_signer() || is_signer);
                    current_account.set_is_writable(current_account.is_writable() || is_writable);
                    // This account is repeated, so there is no need to check for permissions
                    continue;
                }

                let index_in_caller = instruction_context.get_index_of_account_in_instruction(
                    instruction_account.index_in_transaction,
                )?;

                // This unwrap is safe because instruction.accounts.len() == instruction_accounts.len()
                let account_key = &instruction.accounts.get(current_index).unwrap().pubkey;
                // get_index_of_account_in_instruction has already checked if the index is valid.
                let caller_instruction_account = instruction_context
                    .instruction_accounts()
                    .get(index_in_caller as usize)
                    .unwrap();

                // Readonly in caller cannot become writable in callee
                if instruction_account.is_writable() && !caller_instruction_account.is_writable() {
                    ic_msg!(self, "{}'s writable privilege escalated", account_key,);
                    return Err(InstructionError::PrivilegeEscalation);
                }

                // To be signed in the callee,
                // it must be either signed in the caller or by the program
                if instruction_account.is_signer()
                    && !(caller_instruction_account.is_signer() || signers.contains(account_key))
                {
                    ic_msg!(self, "{}'s signer privilege escalated", account_key,);
                    return Err(InstructionError::PrivilegeEscalation);
                }
            }

            // Find and validate executables / program accounts
            let callee_program_id = &instruction.program_id;
            let program_account_index_in_transaction = self
                .transaction_context
                .find_index_of_account(callee_program_id);
            let program_account_index_in_instruction = program_account_index_in_transaction
                .map(|index| instruction_context.get_index_of_account_in_instruction(index));

            // We first check if the account exists in the transaction, and then see if it is part
            // of the instruction.
            if program_account_index_in_instruction.is_none()
                || program_account_index_in_instruction.unwrap().is_err()
            {
                ic_msg!(self, "Unknown program {}", callee_program_id);
                return Err(InstructionError::MissingAccount);
            }

            // SAFETY: This unwrap is safe, because we checked the index in instruction in the
            // previous if-condition.
            program_account_index_in_transaction.unwrap()
        };

        self.transaction_context.configure_next_instruction(
            program_account_index,
            instruction_accounts,
            transaction_callee_map,
            Cow::Owned(instruction.data),
        )?;
        Ok(())
    }

    /// Helper to prepare for process_instruction()/process_precompile() when the instruction is
    /// a top level one
    pub fn prepare_next_top_level_instruction(
        &mut self,
        message: &impl SVMMessage,
        instruction: &SVMInstruction,
        program_account_index: IndexOfAccount,
        data: &'ix_data [u8],
    ) -> Result<(), InstructionError> {
        // We reference accounts by an u8 index, so we have a total of 256 accounts.
        let mut transaction_callee_map: Vec<u16> = vec![u16::MAX; MAX_ACCOUNTS_PER_TRANSACTION];

        let mut instruction_accounts: Vec<InstructionAccount> =
            Vec::with_capacity(instruction.accounts.len());
        for index_in_transaction in instruction.accounts.iter() {
            debug_assert!((*index_in_transaction as usize) < transaction_callee_map.len());

            let index_in_callee = transaction_callee_map
                .get_mut(*index_in_transaction as usize)
                .unwrap();

            if (*index_in_callee as usize) > instruction_accounts.len() {
                *index_in_callee = instruction_accounts.len() as u16;
            }

            let index_in_transaction = *index_in_transaction as usize;
            instruction_accounts.push(InstructionAccount::new(
                index_in_transaction as IndexOfAccount,
                message.is_signer(index_in_transaction),
                message.is_writable(index_in_transaction),
            ));
        }

        self.transaction_context.configure_next_instruction(
            program_account_index,
            instruction_accounts,
            transaction_callee_map,
            Cow::Borrowed(data),
        )?;
        Ok(())
    }

    /// Processes an instruction and returns how many compute units were used
    pub fn process_instruction(
        &mut self,
        compute_units_consumed: &mut u64,
        timings: &mut ExecuteTimings,
    ) -> Result<(), InstructionError> {
        *compute_units_consumed = 0;
        self.push()?;
        self.process_executable_chain(compute_units_consumed, timings)
            // MUST pop if and only if `push` succeeded, independent of `result`.
            // Thus, the `.and()` instead of an `.and_then()`.
            .and(self.pop())
    }

    /// Processes a precompile instruction
    pub fn process_precompile(
        &mut self,
        program_id: &Pubkey,
        instruction_data: &[u8],
        message_instruction_datas_iter: impl Iterator<Item = &'ix_data [u8]>,
    ) -> Result<(), InstructionError> {
        self.push()?;
        let instruction_datas: Vec<_> = message_instruction_datas_iter.collect();
        self.environment_config
            .epoch_stake_callback
            .process_precompile(program_id, instruction_data, instruction_datas)
            .map_err(InstructionError::from)
            .and(self.pop())
    }

    /// Calls the instruction's program entrypoint method
    fn process_executable_chain(
        &mut self,
        compute_units_consumed: &mut u64,
        timings: &mut ExecuteTimings,
    ) -> Result<(), InstructionError> {
        let instruction_context = self.transaction_context.get_current_instruction_context()?;
        let process_executable_chain_time = Measure::start("process_executable_chain_time");

        let builtin_id = {
            let owner_id = instruction_context.get_program_owner()?;
            if native_loader::check_id(&owner_id) {
                *instruction_context.get_program_key()?
            } else if bpf_loader_deprecated::check_id(&owner_id)
                || bpf_loader::check_id(&owner_id)
                || bpf_loader_upgradeable::check_id(&owner_id)
                || loader_v4::check_id(&owner_id)
            {
                owner_id
            } else {
                return Err(InstructionError::UnsupportedProgramId);
            }
        };

        // The Murmur3 hash value (used by RBPF) of the string "entrypoint"
        const ENTRYPOINT_KEY: u32 = 0x71E3CF81;
        let entry = self
            .program_cache_for_tx_batch
            .find(&builtin_id)
            .ok_or(InstructionError::UnsupportedProgramId)?;
        let function = match &entry.program {
            ProgramCacheEntryType::Builtin(program) => program
                .get_function_registry()
                .lookup_by_key(ENTRYPOINT_KEY)
                .map(|(_name, function)| function),
            _ => None,
        }
        .ok_or(InstructionError::UnsupportedProgramId)?;

        let program_id = *instruction_context.get_program_key()?;
        self.transaction_context
            .set_return_data(program_id, Vec::new())?;
        let logger = self.get_log_collector();
        stable_log::program_invoke(&logger, &program_id, self.get_stack_height());
        let pre_remaining_units = self.get_remaining();
        // In program-runtime v2 we will create this VM instance only once per transaction.
        // `program_runtime_environment_v2.get_config()` will be used instead of `mock_config`.
        // For now, only built-ins are invoked from here, so the VM and its Config are irrelevant.
        let mock_config = Config::default();
        let empty_memory_mapping =
            MemoryMapping::new(Vec::new(), &mock_config, SBPFVersion::V0).unwrap();
        let mut vm = EbpfVm::new(
            self.environment_config
                .program_runtime_environments_for_execution
                .program_runtime_v2
                .clone(),
            SBPFVersion::V0,
            // Removes lifetime tracking
            unsafe { std::mem::transmute::<&mut InvokeContext, &mut InvokeContext>(self) },
            empty_memory_mapping,
            0,
        );
        vm.invoke_function(function);
        let result = match vm.program_result {
            ProgramResult::Ok(_) => {
                stable_log::program_success(&logger, &program_id);
                Ok(())
            }
            ProgramResult::Err(ref err) => {
                if let EbpfError::SyscallError(syscall_error) = err {
                    if let Some(instruction_err) = syscall_error.downcast_ref::<InstructionError>()
                    {
                        stable_log::program_failure(&logger, &program_id, instruction_err);
                        Err(instruction_err.clone())
                    } else {
                        stable_log::program_failure(&logger, &program_id, syscall_error);
                        Err(InstructionError::ProgramFailedToComplete)
                    }
                } else {
                    stable_log::program_failure(&logger, &program_id, err);
                    Err(InstructionError::ProgramFailedToComplete)
                }
            }
        };
        let post_remaining_units = self.get_remaining();
        *compute_units_consumed = pre_remaining_units.saturating_sub(post_remaining_units);

        if builtin_id == program_id && result.is_ok() && *compute_units_consumed == 0 {
            return Err(InstructionError::BuiltinProgramsMustConsumeComputeUnits);
        }

        timings
            .execute_accessories
            .process_instructions
            .process_executable_chain_us += process_executable_chain_time.end_as_us();
        result
    }

    /// Get this invocation's LogCollector
    pub fn get_log_collector(&self) -> Option<Rc<RefCell<LogCollector>>> {
        self.log_collector.clone()
    }

    /// Consume compute units
    pub fn consume_checked(&self, amount: u64) -> Result<(), Box<dyn std::error::Error>> {
        let mut compute_meter = self.compute_meter.borrow_mut();
        let exceeded = *compute_meter < amount;
        *compute_meter = compute_meter.saturating_sub(amount);
        if exceeded {
            return Err(Box::new(InstructionError::ComputationalBudgetExceeded));
        }
        Ok(())
    }

    /// Set compute units
    ///
    /// Only use for tests and benchmarks
    pub fn mock_set_remaining(&self, remaining: u64) {
        *self.compute_meter.borrow_mut() = remaining;
    }

    /// Get this invocation's compute budget
    pub fn get_compute_budget(&self) -> &SVMTransactionExecutionBudget {
        &self.compute_budget
    }

    /// Get this invocation's compute budget
    pub fn get_execution_cost(&self) -> &SVMTransactionExecutionCost {
        &self.execution_cost
    }

    /// Get the current feature set.
    pub fn get_feature_set(&self) -> &SVMFeatureSet {
        self.environment_config.feature_set
    }

    pub fn get_program_runtime_environments_for_deployment(&self) -> &ProgramRuntimeEnvironments {
        self.environment_config
            .program_runtime_environments_for_deployment
    }

    pub fn is_stake_raise_minimum_delegation_to_1_sol_active(&self) -> bool {
        self.environment_config
            .feature_set
            .stake_raise_minimum_delegation_to_1_sol
    }

    pub fn is_deprecate_legacy_vote_ixs_active(&self) -> bool {
        self.environment_config
            .feature_set
            .deprecate_legacy_vote_ixs
    }

    /// Get cached sysvars
    pub fn get_sysvar_cache(&self) -> &SysvarCache {
        self.environment_config.sysvar_cache
    }

    /// Get cached epoch total stake.
    pub fn get_epoch_stake(&self) -> u64 {
        self.environment_config
            .epoch_stake_callback
            .get_epoch_stake()
    }

    /// Get cached stake for the epoch vote account.
    pub fn get_epoch_stake_for_vote_account(&self, pubkey: &'a Pubkey) -> u64 {
        self.environment_config
            .epoch_stake_callback
            .get_epoch_stake_for_vote_account(pubkey)
    }

    pub fn is_precompile(&self, pubkey: &Pubkey) -> bool {
        self.environment_config
            .epoch_stake_callback
            .is_precompile(pubkey)
    }

    // Should alignment be enforced during user pointer translation
    pub fn get_check_aligned(&self) -> bool {
        self.transaction_context
            .get_current_instruction_context()
            .and_then(|instruction_context| {
                let owner_id = instruction_context.get_program_owner();
                debug_assert!(owner_id.is_ok());
                owner_id
            })
            .map(|owner_key| owner_key != bpf_loader_deprecated::id())
            .unwrap_or(true)
    }

    // Set this instruction syscall context
    pub fn set_syscall_context(
        &mut self,
        syscall_context: SyscallContext,
    ) -> Result<(), InstructionError> {
        *self
            .syscall_context
            .last_mut()
            .ok_or(InstructionError::CallDepth)? = Some(syscall_context);
        Ok(())
    }

    // Get this instruction's SyscallContext
    pub fn get_syscall_context(&self) -> Result<&SyscallContext, InstructionError> {
        self.syscall_context
            .last()
            .and_then(std::option::Option::as_ref)
            .ok_or(InstructionError::CallDepth)
    }

    // Get this instruction's SyscallContext
    pub fn get_syscall_context_mut(&mut self) -> Result<&mut SyscallContext, InstructionError> {
        self.syscall_context
            .last_mut()
            .and_then(|syscall_context| syscall_context.as_mut())
            .ok_or(InstructionError::CallDepth)
    }

    /// Insert a VM register trace
    pub fn insert_register_trace(&mut self, register_trace: Vec<[u64; 12]>) {
        if register_trace.is_empty() {
            return;
        }
        let Ok(instruction_context) = self.transaction_context.get_current_instruction_context()
        else {
            return;
        };
        self.register_traces
            .push((instruction_context.get_index_in_trace(), register_trace));
    }

    /// Iterates over all VM register traces (including CPI)
    pub fn iterate_vm_traces(
        &self,
        callback: &dyn Fn(InstructionContext, &Executable, RegisterTrace),
    ) {
        for (index_in_trace, register_trace) in &self.register_traces {
            let Ok(instruction_context) = self
                .transaction_context
                .get_instruction_context_at_index_in_trace(*index_in_trace)
            else {
                continue;
            };
            let Ok(program_id) = instruction_context.get_program_key() else {
                continue;
            };
            let Some(entry) = self.program_cache_for_tx_batch.find(program_id) else {
                continue;
            };
            let ProgramCacheEntryType::Loaded(ref executable) = entry.program else {
                continue;
            };
            callback(instruction_context, executable, register_trace.as_slice());
        }
    }
}

#[macro_export]
macro_rules! with_mock_invoke_context_with_feature_set {
    (
        $invoke_context:ident,
        $transaction_context:ident,
        $feature_set:ident,
        $transaction_accounts:expr $(,)?
    ) => {
        use {
            solana_svm_callback::InvokeContextCallback,
            solana_svm_log_collector::LogCollector,
            $crate::{
                __private::{Hash, ReadableAccount, Rent, TransactionContext},
                execution_budget::{SVMTransactionExecutionBudget, SVMTransactionExecutionCost},
                invoke_context::{EnvironmentConfig, InvokeContext},
                loaded_programs::{ProgramCacheForTxBatch, ProgramRuntimeEnvironments},
                sysvar_cache::SysvarCache,
            },
        };

        struct MockInvokeContextCallback {}
        impl InvokeContextCallback for MockInvokeContextCallback {}

        let compute_budget = SVMTransactionExecutionBudget::new_with_defaults(
            $feature_set.raise_cpi_nesting_limit_to_8,
        );
        let mut $transaction_context = TransactionContext::new(
            $transaction_accounts,
            Rent::default(),
            compute_budget.max_instruction_stack_depth,
            compute_budget.max_instruction_trace_length,
        );
        let mut sysvar_cache = SysvarCache::default();
        sysvar_cache.fill_missing_entries(|pubkey, callback| {
            for index in 0..$transaction_context.get_number_of_accounts() {
                if $transaction_context
                    .get_key_of_account_at_index(index)
                    .unwrap()
                    == pubkey
                {
                    callback(
                        $transaction_context
                            .accounts()
                            .try_borrow(index)
                            .unwrap()
                            .data(),
                    );
                }
            }
        });
        let program_runtime_environments = ProgramRuntimeEnvironments::default();
        let environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockInvokeContextCallback {},
            $feature_set,
            &program_runtime_environments,
            &program_runtime_environments,
            &sysvar_cache,
        );
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        let mut $invoke_context = InvokeContext::new(
            &mut $transaction_context,
            &mut program_cache_for_tx_batch,
            environment_config,
            Some(LogCollector::new_ref()),
            compute_budget,
            SVMTransactionExecutionCost::new_with_defaults(
                $feature_set.increase_cpi_account_info_limit,
            ),
        );
    };
}

#[macro_export]
macro_rules! with_mock_invoke_context {
    (
        $invoke_context:ident,
        $transaction_context:ident,
        $transaction_accounts:expr $(,)?
    ) => {
        use $crate::with_mock_invoke_context_with_feature_set;
        let feature_set = &solana_svm_feature_set::SVMFeatureSet::default();
        with_mock_invoke_context_with_feature_set!(
            $invoke_context,
            $transaction_context,
            feature_set,
            $transaction_accounts
        )
    };
}

#[allow(clippy::too_many_arguments)]
pub fn mock_process_instruction_with_feature_set<
    F: FnMut(&mut InvokeContext),
    G: FnMut(&mut InvokeContext),
>(
    loader_id: &Pubkey,
    program_index: Option<IndexOfAccount>,
    instruction_data: &[u8],
    mut transaction_accounts: Vec<KeyedAccountSharedData>,
    instruction_account_metas: Vec<AccountMeta>,
    expected_result: Result<(), InstructionError>,
    builtin_function: BuiltinFunctionWithContext,
    mut pre_adjustments: F,
    mut post_adjustments: G,
    feature_set: &SVMFeatureSet,
) -> Vec<AccountSharedData> {
    let mut instruction_accounts: Vec<InstructionAccount> =
        Vec::with_capacity(instruction_account_metas.len());
    for account_meta in instruction_account_metas.iter() {
        let index_in_transaction = transaction_accounts
            .iter()
            .position(|(key, _account)| *key == account_meta.pubkey)
            .unwrap_or(transaction_accounts.len())
            as IndexOfAccount;
        instruction_accounts.push(InstructionAccount::new(
            index_in_transaction,
            account_meta.is_signer,
            account_meta.is_writable,
        ));
    }

    let program_index = if let Some(index) = program_index {
        index
    } else {
        let processor_account = AccountSharedData::new(0, 0, &native_loader::id());
        transaction_accounts.push((*loader_id, processor_account));
        transaction_accounts.len().saturating_sub(1) as IndexOfAccount
    };
    let pop_epoch_schedule_account = if !transaction_accounts
        .iter()
        .any(|(key, _)| *key == sysvar::epoch_schedule::id())
    {
        transaction_accounts.push((
            sysvar::epoch_schedule::id(),
            create_account_shared_data_for_test(&EpochSchedule::default()),
        ));
        true
    } else {
        false
    };
    with_mock_invoke_context_with_feature_set!(
        invoke_context,
        transaction_context,
        feature_set,
        transaction_accounts
    );
    let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
    program_cache_for_tx_batch.replenish(
        *loader_id,
        Arc::new(ProgramCacheEntry::new_builtin(0, 0, builtin_function)),
    );
    program_cache_for_tx_batch.set_slot_for_tests(
        invoke_context
            .get_sysvar_cache()
            .get_clock()
            .map(|clock| clock.slot)
            .unwrap_or(1),
    );
    invoke_context.program_cache_for_tx_batch = &mut program_cache_for_tx_batch;
    pre_adjustments(&mut invoke_context);
    invoke_context
        .transaction_context
        .configure_next_instruction_for_tests(
            program_index,
            instruction_accounts,
            instruction_data.to_vec(),
        )
        .unwrap();
    let result = invoke_context.process_instruction(&mut 0, &mut ExecuteTimings::default());
    assert_eq!(result, expected_result);
    post_adjustments(&mut invoke_context);
    let mut transaction_accounts = transaction_context.deconstruct_without_keys().unwrap();
    if pop_epoch_schedule_account {
        transaction_accounts.pop();
    }
    transaction_accounts.pop();
    transaction_accounts
}

pub fn mock_process_instruction<F: FnMut(&mut InvokeContext), G: FnMut(&mut InvokeContext)>(
    loader_id: &Pubkey,
    program_index: Option<IndexOfAccount>,
    instruction_data: &[u8],
    transaction_accounts: Vec<KeyedAccountSharedData>,
    instruction_account_metas: Vec<AccountMeta>,
    expected_result: Result<(), InstructionError>,
    builtin_function: BuiltinFunctionWithContext,
    pre_adjustments: F,
    post_adjustments: G,
) -> Vec<AccountSharedData> {
    mock_process_instruction_with_feature_set(
        loader_id,
        program_index,
        instruction_data,
        transaction_accounts,
        instruction_account_metas,
        expected_result,
        builtin_function,
        pre_adjustments,
        post_adjustments,
        &SVMFeatureSet::all_enabled(),
    )
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
        serde::{Deserialize, Serialize},
        solana_account::WritableAccount,
        solana_instruction::Instruction,
        solana_keypair::Keypair,
        solana_rent::Rent,
        solana_signer::Signer,
        solana_transaction::{sanitized::SanitizedTransaction, Transaction},
        solana_transaction_context::MAX_ACCOUNTS_PER_INSTRUCTION,
        std::collections::HashSet,
        test_case::test_case,
    };

    #[derive(Debug, Serialize, Deserialize)]
    enum MockInstruction {
        NoopSuccess,
        NoopFail,
        ModifyOwned,
        ModifyNotOwned,
        ModifyReadonly,
        UnbalancedPush,
        UnbalancedPop,
        ConsumeComputeUnits {
            compute_units_to_consume: u64,
            desired_result: Result<(), InstructionError>,
        },
        Resize {
            new_len: u64,
        },
    }

    const MOCK_BUILTIN_COMPUTE_UNIT_COST: u64 = 1;

    declare_process_instruction!(
        MockBuiltin,
        MOCK_BUILTIN_COMPUTE_UNIT_COST,
        |invoke_context| {
            let transaction_context = &invoke_context.transaction_context;
            let instruction_context = transaction_context.get_current_instruction_context()?;
            let instruction_data = instruction_context.get_instruction_data();
            let program_id = instruction_context.get_program_key()?;
            let instruction_accounts = (0..4)
                .map(|instruction_account_index| {
                    InstructionAccount::new(instruction_account_index, false, false)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                program_id,
                instruction_context
                    .try_borrow_instruction_account(0)?
                    .get_owner()
            );
            assert_ne!(
                instruction_context
                    .try_borrow_instruction_account(1)?
                    .get_owner(),
                instruction_context.get_key_of_instruction_account(0)?
            );

            if let Ok(instruction) = bincode::deserialize(instruction_data) {
                match instruction {
                    MockInstruction::NoopSuccess => (),
                    MockInstruction::NoopFail => return Err(InstructionError::GenericError),
                    MockInstruction::ModifyOwned => instruction_context
                        .try_borrow_instruction_account(0)?
                        .set_data_from_slice(&[1])?,
                    MockInstruction::ModifyNotOwned => instruction_context
                        .try_borrow_instruction_account(1)?
                        .set_data_from_slice(&[1])?,
                    MockInstruction::ModifyReadonly => instruction_context
                        .try_borrow_instruction_account(2)?
                        .set_data_from_slice(&[1])?,
                    MockInstruction::UnbalancedPush => {
                        instruction_context
                            .try_borrow_instruction_account(0)?
                            .checked_add_lamports(1)?;
                        let program_id = *transaction_context.get_key_of_account_at_index(3)?;
                        let metas = vec![
                            AccountMeta::new_readonly(
                                *transaction_context.get_key_of_account_at_index(0)?,
                                false,
                            ),
                            AccountMeta::new_readonly(
                                *transaction_context.get_key_of_account_at_index(1)?,
                                false,
                            ),
                        ];
                        let inner_instruction = Instruction::new_with_bincode(
                            program_id,
                            &MockInstruction::NoopSuccess,
                            metas,
                        );
                        invoke_context
                            .transaction_context
                            .configure_next_instruction_for_tests(3, instruction_accounts, vec![])
                            .unwrap();
                        let result = invoke_context.push();
                        assert_eq!(result, Err(InstructionError::UnbalancedInstruction));
                        result?;
                        invoke_context
                            .native_invoke(inner_instruction, &[])
                            .and(invoke_context.pop())?;
                    }
                    MockInstruction::UnbalancedPop => instruction_context
                        .try_borrow_instruction_account(0)?
                        .checked_add_lamports(1)?,
                    MockInstruction::ConsumeComputeUnits {
                        compute_units_to_consume,
                        desired_result,
                    } => {
                        invoke_context
                            .consume_checked(compute_units_to_consume)
                            .map_err(|_| InstructionError::ComputationalBudgetExceeded)?;
                        return desired_result;
                    }
                    MockInstruction::Resize { new_len } => instruction_context
                        .try_borrow_instruction_account(0)?
                        .set_data_from_slice(&vec![0; new_len as usize])?,
                }
            } else {
                return Err(InstructionError::InvalidInstructionData);
            }
            Ok(())
        }
    );

    #[test_case(false; "SIMD-0268 disabled")]
    #[test_case(true; "SIMD-0268 enabled")]
    fn test_instruction_stack_height(simd_0268_active: bool) {
        let one_more_than_max_depth =
            SVMTransactionExecutionBudget::new_with_defaults(simd_0268_active)
                .max_instruction_stack_depth
                .saturating_add(1);
        let mut invoke_stack = vec![];
        let mut transaction_accounts = vec![];
        let mut instruction_accounts = vec![];
        for index in 0..one_more_than_max_depth {
            invoke_stack.push(solana_pubkey::new_rand());
            transaction_accounts.push((
                solana_pubkey::new_rand(),
                AccountSharedData::new(index as u64, 1, invoke_stack.get(index).unwrap()),
            ));
            instruction_accounts.push(InstructionAccount::new(
                index as IndexOfAccount,
                false,
                true,
            ));
        }
        for (index, program_id) in invoke_stack.iter().enumerate() {
            transaction_accounts.push((
                *program_id,
                AccountSharedData::new(1, 1, &solana_pubkey::Pubkey::default()),
            ));
            instruction_accounts.push(InstructionAccount::new(
                index as IndexOfAccount,
                false,
                false,
            ));
        }
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);

        // Check call depth increases and has a limit
        let mut depth_reached: usize = 0;
        for _ in 0..invoke_stack.len() {
            invoke_context
                .transaction_context
                .configure_next_instruction_for_tests(
                    one_more_than_max_depth.saturating_add(depth_reached) as IndexOfAccount,
                    instruction_accounts.clone(),
                    vec![],
                )
                .unwrap();
            if Err(InstructionError::CallDepth) == invoke_context.push() {
                break;
            }
            depth_reached = depth_reached.saturating_add(1);
        }
        assert_ne!(depth_reached, 0);
        assert!(depth_reached < one_more_than_max_depth);
    }

    #[test]
    fn test_max_instruction_trace_length() {
        const MAX_INSTRUCTIONS: usize = 8;
        let mut transaction_context = TransactionContext::new(
            vec![(
                Pubkey::new_unique(),
                AccountSharedData::new(1, 1, &Pubkey::new_unique()),
            )],
            Rent::default(),
            1,
            MAX_INSTRUCTIONS,
        );
        for _ in 0..MAX_INSTRUCTIONS {
            transaction_context.push().unwrap();
            transaction_context
                .configure_next_instruction_for_tests(
                    0,
                    vec![InstructionAccount::new(0, false, false)],
                    vec![],
                )
                .unwrap();
            transaction_context.pop().unwrap();
        }
        assert_eq!(
            transaction_context.push(),
            Err(InstructionError::MaxInstructionTraceLengthExceeded)
        );
    }

    #[test_case(MockInstruction::NoopSuccess, Ok(()); "NoopSuccess")]
    #[test_case(MockInstruction::NoopFail, Err(InstructionError::GenericError); "NoopFail")]
    #[test_case(MockInstruction::ModifyOwned, Ok(()); "ModifyOwned")]
    #[test_case(MockInstruction::ModifyNotOwned, Err(InstructionError::ExternalAccountDataModified); "ModifyNotOwned")]
    #[test_case(MockInstruction::ModifyReadonly, Err(InstructionError::ReadonlyDataModified); "ModifyReadonly")]
    #[test_case(MockInstruction::UnbalancedPush, Err(InstructionError::UnbalancedInstruction); "UnbalancedPush")]
    #[test_case(MockInstruction::UnbalancedPop, Err(InstructionError::UnbalancedInstruction); "UnbalancedPop")]
    fn test_process_instruction_account_modifications(
        instruction: MockInstruction,
        expected_result: Result<(), InstructionError>,
    ) {
        let callee_program_id = solana_pubkey::new_rand();
        let owned_account = AccountSharedData::new(42, 1, &callee_program_id);
        let not_owned_account = AccountSharedData::new(84, 1, &solana_pubkey::new_rand());
        let readonly_account = AccountSharedData::new(168, 1, &solana_pubkey::new_rand());
        let loader_account = AccountSharedData::new(0, 1, &native_loader::id());
        let mut program_account = AccountSharedData::new(1, 1, &native_loader::id());
        program_account.set_executable(true);
        let transaction_accounts = vec![
            (solana_pubkey::new_rand(), owned_account),
            (solana_pubkey::new_rand(), not_owned_account),
            (solana_pubkey::new_rand(), readonly_account),
            (callee_program_id, program_account),
            (solana_pubkey::new_rand(), loader_account),
        ];
        let metas = vec![
            AccountMeta::new(transaction_accounts.first().unwrap().0, false),
            AccountMeta::new(transaction_accounts.get(1).unwrap().0, false),
            AccountMeta::new_readonly(transaction_accounts.get(2).unwrap().0, false),
        ];
        let instruction_accounts = (0..4)
            .map(|instruction_account_index| {
                InstructionAccount::new(
                    instruction_account_index,
                    false,
                    instruction_account_index < 2,
                )
            })
            .collect::<Vec<_>>();
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        program_cache_for_tx_batch.replenish(
            callee_program_id,
            Arc::new(ProgramCacheEntry::new_builtin(0, 1, MockBuiltin::vm)),
        );
        invoke_context.program_cache_for_tx_batch = &mut program_cache_for_tx_batch;

        // Account modification tests
        invoke_context
            .transaction_context
            .configure_next_instruction_for_tests(4, instruction_accounts, vec![])
            .unwrap();
        invoke_context.push().unwrap();
        let inner_instruction =
            Instruction::new_with_bincode(callee_program_id, &instruction, metas.clone());
        let result = invoke_context
            .native_invoke(inner_instruction, &[])
            .and(invoke_context.pop());
        assert_eq!(result, expected_result);
    }

    #[test_case(Ok(()); "Ok")]
    #[test_case(Err(InstructionError::GenericError); "GenericError")]
    fn test_process_instruction_compute_unit_consumption(
        expected_result: Result<(), InstructionError>,
    ) {
        let callee_program_id = solana_pubkey::new_rand();
        let owned_account = AccountSharedData::new(42, 1, &callee_program_id);
        let not_owned_account = AccountSharedData::new(84, 1, &solana_pubkey::new_rand());
        let readonly_account = AccountSharedData::new(168, 1, &solana_pubkey::new_rand());
        let loader_account = AccountSharedData::new(0, 1, &native_loader::id());
        let mut program_account = AccountSharedData::new(1, 1, &native_loader::id());
        program_account.set_executable(true);
        let transaction_accounts = vec![
            (solana_pubkey::new_rand(), owned_account),
            (solana_pubkey::new_rand(), not_owned_account),
            (solana_pubkey::new_rand(), readonly_account),
            (callee_program_id, program_account),
            (solana_pubkey::new_rand(), loader_account),
        ];
        let metas = vec![
            AccountMeta::new(transaction_accounts.first().unwrap().0, false),
            AccountMeta::new(transaction_accounts.get(1).unwrap().0, false),
            AccountMeta::new_readonly(transaction_accounts.get(2).unwrap().0, false),
        ];
        let instruction_accounts = (0..4)
            .map(|instruction_account_index| {
                InstructionAccount::new(
                    instruction_account_index,
                    false,
                    instruction_account_index < 2,
                )
            })
            .collect::<Vec<_>>();
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        program_cache_for_tx_batch.replenish(
            callee_program_id,
            Arc::new(ProgramCacheEntry::new_builtin(0, 1, MockBuiltin::vm)),
        );
        invoke_context.program_cache_for_tx_batch = &mut program_cache_for_tx_batch;

        // Compute unit consumption tests
        let compute_units_to_consume = 10;
        invoke_context
            .transaction_context
            .configure_next_instruction_for_tests(4, instruction_accounts, vec![])
            .unwrap();
        invoke_context.push().unwrap();
        let inner_instruction = Instruction::new_with_bincode(
            callee_program_id,
            &MockInstruction::ConsumeComputeUnits {
                compute_units_to_consume,
                desired_result: expected_result.clone(),
            },
            metas.clone(),
        );
        invoke_context
            .prepare_next_instruction(inner_instruction, &[])
            .unwrap();

        let mut compute_units_consumed = 0;
        let result = invoke_context
            .process_instruction(&mut compute_units_consumed, &mut ExecuteTimings::default());

        // Because the instruction had compute cost > 0, then regardless of the execution result,
        // the number of compute units consumed should be a non-default which is something greater
        // than zero.
        assert!(compute_units_consumed > 0);
        assert_eq!(
            compute_units_consumed,
            compute_units_to_consume.saturating_add(MOCK_BUILTIN_COMPUTE_UNIT_COST),
        );
        assert_eq!(result, expected_result);

        invoke_context.pop().unwrap();
    }

    #[test]
    fn test_invoke_context_compute_budget() {
        let transaction_accounts = vec![(solana_pubkey::new_rand(), AccountSharedData::default())];
        let execution_budget = SVMTransactionExecutionBudget {
            compute_unit_limit: u64::from(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT),
            ..SVMTransactionExecutionBudget::default()
        };

        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        invoke_context.compute_budget = execution_budget;

        invoke_context
            .transaction_context
            .configure_next_instruction_for_tests(0, vec![], vec![])
            .unwrap();
        invoke_context.push().unwrap();
        assert_eq!(*invoke_context.get_compute_budget(), execution_budget);
        invoke_context.pop().unwrap();
    }

    #[test_case(0; "Resize the account to *the same size*, so not consuming any additional size")]
    #[test_case(1; "Resize the account larger")]
    #[test_case(-1; "Resize the account smaller")]
    fn test_process_instruction_accounts_resize_delta(resize_delta: i64) {
        let program_key = Pubkey::new_unique();
        let user_account_data_len = 123u64;
        let user_account =
            AccountSharedData::new(100, user_account_data_len as usize, &program_key);
        let dummy_account = AccountSharedData::new(10, 0, &program_key);
        let mut program_account = AccountSharedData::new(500, 500, &native_loader::id());
        program_account.set_executable(true);
        let transaction_accounts = vec![
            (Pubkey::new_unique(), user_account),
            (Pubkey::new_unique(), dummy_account),
            (program_key, program_account),
        ];
        let instruction_accounts = vec![
            InstructionAccount::new(0, false, true),
            InstructionAccount::new(1, false, false),
        ];
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        program_cache_for_tx_batch.replenish(
            program_key,
            Arc::new(ProgramCacheEntry::new_builtin(0, 0, MockBuiltin::vm)),
        );
        invoke_context.program_cache_for_tx_batch = &mut program_cache_for_tx_batch;

        let new_len = (user_account_data_len as i64).saturating_add(resize_delta) as u64;
        let instruction_data = bincode::serialize(&MockInstruction::Resize { new_len }).unwrap();

        invoke_context
            .transaction_context
            .configure_next_instruction_for_tests(2, instruction_accounts, instruction_data)
            .unwrap();
        let result = invoke_context.process_instruction(&mut 0, &mut ExecuteTimings::default());

        assert!(result.is_ok());
        assert_eq!(
            invoke_context.transaction_context.accounts().resize_delta(),
            resize_delta
        );
    }

    #[test]
    fn test_prepare_instruction_maximum_accounts() {
        const MAX_ACCOUNTS_REFERENCED: usize = u16::MAX as usize;
        let mut transaction_accounts: Vec<KeyedAccountSharedData> =
            Vec::with_capacity(MAX_ACCOUNTS_PER_TRANSACTION);
        let mut account_metas: Vec<AccountMeta> = Vec::with_capacity(MAX_ACCOUNTS_REFERENCED);

        // Fee-payer
        let fee_payer = Keypair::new();
        transaction_accounts.push((
            fee_payer.pubkey(),
            AccountSharedData::new(1, 1, &Pubkey::new_unique()),
        ));
        account_metas.push(AccountMeta::new(fee_payer.pubkey(), true));

        let program_id = Pubkey::new_unique();
        let mut program_account = AccountSharedData::new(1, 1, &Pubkey::new_unique());
        program_account.set_executable(true);
        transaction_accounts.push((program_id, program_account));
        account_metas.push(AccountMeta::new_readonly(program_id, false));

        for i in 2..MAX_ACCOUNTS_REFERENCED {
            // Let's reference 256 unique accounts, and the rest is repeated.
            if i < MAX_ACCOUNTS_PER_TRANSACTION {
                let key = Pubkey::new_unique();
                transaction_accounts
                    .push((key, AccountSharedData::new(1, 1, &Pubkey::new_unique())));
                account_metas.push(AccountMeta::new_readonly(key, false));
            } else {
                let repeated_key = transaction_accounts
                    .get(i % MAX_ACCOUNTS_PER_TRANSACTION)
                    .unwrap()
                    .0;
                account_metas.push(AccountMeta::new_readonly(repeated_key, false));
            }
        }

        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);

        let instruction_1 = Instruction::new_with_bytes(program_id, &[20], account_metas.clone());

        let instruction_2 = Instruction::new_with_bytes(
            program_id,
            &[20],
            account_metas.iter().rev().cloned().collect(),
        );

        let transaction = Transaction::new_with_payer(
            &[instruction_1.clone(), instruction_2.clone()],
            Some(&fee_payer.pubkey()),
        );

        let sanitized =
            SanitizedTransaction::try_from_legacy_transaction(transaction, &HashSet::new())
                .unwrap();

        fn test_case_1(invoke_context: &InvokeContext) {
            let instruction_context = invoke_context
                .transaction_context
                .get_next_instruction_context()
                .unwrap();
            for index_in_instruction in 0..MAX_ACCOUNTS_REFERENCED as IndexOfAccount {
                let index_in_transaction = instruction_context
                    .get_index_of_instruction_account_in_transaction(index_in_instruction)
                    .unwrap();
                let other_ix_index = instruction_context
                    .get_index_of_account_in_instruction(index_in_transaction)
                    .unwrap();
                if (index_in_instruction as usize) < MAX_ACCOUNTS_PER_TRANSACTION {
                    assert_eq!(index_in_instruction, index_in_transaction);
                    assert_eq!(index_in_instruction, other_ix_index);
                } else {
                    assert_eq!(
                        index_in_instruction as usize % MAX_ACCOUNTS_PER_TRANSACTION,
                        index_in_transaction as usize
                    );
                    assert_eq!(
                        index_in_instruction as usize % MAX_ACCOUNTS_PER_TRANSACTION,
                        other_ix_index as usize
                    );
                }
            }
        }

        fn test_case_2(invoke_context: &InvokeContext) {
            let instruction_context = invoke_context
                .transaction_context
                .get_next_instruction_context()
                .unwrap();
            for index_in_instruction in 0..MAX_ACCOUNTS_REFERENCED as IndexOfAccount {
                let index_in_transaction = instruction_context
                    .get_index_of_instruction_account_in_transaction(index_in_instruction)
                    .unwrap();
                let other_ix_index = instruction_context
                    .get_index_of_account_in_instruction(index_in_transaction)
                    .unwrap();
                assert_eq!(
                    index_in_transaction,
                    (MAX_ACCOUNTS_REFERENCED as u16)
                        .saturating_sub(index_in_instruction)
                        .saturating_sub(1)
                        .overflowing_rem(MAX_ACCOUNTS_PER_TRANSACTION as u16)
                        .0
                );
                if (index_in_instruction as usize) < MAX_ACCOUNTS_PER_TRANSACTION {
                    assert_eq!(index_in_instruction, other_ix_index);
                } else {
                    assert_eq!(
                        index_in_instruction as usize % MAX_ACCOUNTS_PER_TRANSACTION,
                        other_ix_index as usize
                    );
                }
            }
        }

        let svm_instruction =
            SVMInstruction::from(sanitized.message().instructions().first().unwrap());
        invoke_context
            .prepare_next_top_level_instruction(
                &sanitized,
                &svm_instruction,
                90,
                svm_instruction.data,
            )
            .unwrap();

        test_case_1(&invoke_context);

        invoke_context.transaction_context.push().unwrap();
        let svm_instruction =
            SVMInstruction::from(sanitized.message().instructions().get(1).unwrap());
        invoke_context
            .prepare_next_top_level_instruction(
                &sanitized,
                &svm_instruction,
                90,
                svm_instruction.data,
            )
            .unwrap();

        test_case_2(&invoke_context);

        invoke_context.transaction_context.push().unwrap();
        invoke_context
            .prepare_next_instruction(instruction_1, &[fee_payer.pubkey()])
            .unwrap();
        test_case_1(&invoke_context);

        invoke_context.transaction_context.push().unwrap();
        invoke_context
            .prepare_next_instruction(instruction_2, &[fee_payer.pubkey()])
            .unwrap();
        test_case_2(&invoke_context);
    }

    #[test]
    fn test_duplicated_accounts() {
        let mut transaction_accounts: Vec<KeyedAccountSharedData> =
            Vec::with_capacity(MAX_ACCOUNTS_PER_TRANSACTION);
        let mut account_metas: Vec<AccountMeta> =
            Vec::with_capacity(MAX_ACCOUNTS_PER_INSTRUCTION.saturating_sub(1));

        // Fee-payer
        let fee_payer = Keypair::new();
        transaction_accounts.push((
            fee_payer.pubkey(),
            AccountSharedData::new(1, 1, &Pubkey::new_unique()),
        ));
        account_metas.push(AccountMeta::new(fee_payer.pubkey(), true));

        let program_id = Pubkey::new_unique();
        let mut program_account = AccountSharedData::new(1, 1, &Pubkey::new_unique());
        program_account.set_executable(true);
        transaction_accounts.push((program_id, program_account));
        account_metas.push(AccountMeta::new_readonly(program_id, false));

        for i in 2..account_metas.capacity() {
            if i % 2 == 0 {
                let key = Pubkey::new_unique();
                transaction_accounts
                    .push((key, AccountSharedData::new(1, 1, &Pubkey::new_unique())));
                account_metas.push(AccountMeta::new_readonly(key, false));
            } else {
                let last_key = transaction_accounts.last().unwrap().0;
                account_metas.push(AccountMeta::new_readonly(last_key, false));
            }
        }

        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);

        let instruction = Instruction::new_with_bytes(program_id, &[20], account_metas.clone());

        let transaction = Transaction::new_with_payer(&[instruction], Some(&fee_payer.pubkey()));

        let sanitized =
            SanitizedTransaction::try_from_legacy_transaction(transaction, &HashSet::new())
                .unwrap();
        let svm_instruction =
            SVMInstruction::from(sanitized.message().instructions().first().unwrap());

        invoke_context
            .prepare_next_top_level_instruction(
                &sanitized,
                &svm_instruction,
                90,
                svm_instruction.data,
            )
            .unwrap();

        {
            let instruction_context = invoke_context
                .transaction_context
                .get_next_instruction_context()
                .unwrap();
            for index_in_instruction in 2..account_metas.len() as IndexOfAccount {
                let is_duplicate = instruction_context
                    .is_instruction_account_duplicate(index_in_instruction)
                    .unwrap();
                if index_in_instruction % 2 == 0 {
                    assert!(is_duplicate.is_none());
                } else {
                    assert_eq!(is_duplicate, Some(index_in_instruction.saturating_sub(1)));
                }
            }
        }

        invoke_context.transaction_context.push().unwrap();

        let instruction = Instruction::new_with_bytes(
            program_id,
            &[20],
            account_metas.iter().cloned().rev().collect(),
        );

        invoke_context
            .prepare_next_instruction(instruction, &[fee_payer.pubkey()])
            .unwrap();
        let instruction_context = invoke_context
            .transaction_context
            .get_next_instruction_context()
            .unwrap();
        for index_in_instruction in 2..account_metas.len().saturating_sub(1) as u16 {
            let is_duplicate = instruction_context
                .is_instruction_account_duplicate(index_in_instruction)
                .unwrap();
            if index_in_instruction % 2 == 0 {
                assert!(is_duplicate.is_none());
            } else {
                assert_eq!(is_duplicate, Some(index_in_instruction.saturating_sub(1)));
            }
        }
    }
}
