#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
pub use self::{
    cpi::{SyscallInvokeSignedC, SyscallInvokeSignedRust},
    logging::{
        SyscallLog, SyscallLogBpfComputeUnits, SyscallLogData, SyscallLogPubkey, SyscallLogU64,
    },
    mem_ops::{SyscallMemcmp, SyscallMemcpy, SyscallMemmove, SyscallMemset},
    sysvar::{
        SyscallGetClockSysvar, SyscallGetEpochRewardsSysvar, SyscallGetEpochScheduleSysvar,
        SyscallGetFeesSysvar, SyscallGetLastRestartSlotSysvar, SyscallGetRentSysvar,
        SyscallGetSysvar,
    },
};
use solana_program_runtime::memory::translate_vm_slice;
#[allow(deprecated)]
use {
    crate::mem_ops::is_nonoverlapping,
    solana_big_mod_exp::{big_mod_exp, BigModExpParams},
    solana_blake3_hasher as blake3,
    solana_cpi::MAX_RETURN_DATA,
    solana_hash::Hash,
    solana_instruction::{error::InstructionError, AccountMeta, ProcessedSiblingInstruction},
    solana_keccak_hasher as keccak, solana_poseidon as poseidon,
    solana_program_entrypoint::{BPF_ALIGN_OF_U128, SUCCESS},
    solana_program_runtime::{
        cpi::CpiError,
        execution_budget::{SVMTransactionExecutionBudget, SVMTransactionExecutionCost},
        invoke_context::InvokeContext,
        memory::MemoryTranslationError,
        stable_log, translate_inner, translate_slice_inner, translate_type_inner,
    },
    solana_pubkey::{Pubkey, PubkeyError, MAX_SEEDS, MAX_SEED_LEN, PUBKEY_BYTES},
    solana_sbpf::{
        declare_builtin_function,
        memory_region::{AccessType, MemoryMapping},
        program::{BuiltinProgram, SBPFVersion},
        vm::Config,
    },
    solana_secp256k1_recover::{
        Secp256k1RecoverError, SECP256K1_PUBLIC_KEY_LENGTH, SECP256K1_SIGNATURE_LENGTH,
    },
    solana_sha256_hasher::Hasher,
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_log_collector::{ic_logger_msg, ic_msg},
    solana_svm_type_overrides::sync::Arc,
    solana_sysvar::SysvarSerialize,
    solana_transaction_context::vm_slice::VmSlice,
    std::{
        alloc::Layout,
        mem::{align_of, size_of},
        slice::from_raw_parts_mut,
        str::{from_utf8, Utf8Error},
    },
    thiserror::Error as ThisError,
};

mod cpi;
mod logging;
mod mem_ops;
mod sysvar;

/// Error definitions
#[derive(Debug, ThisError, PartialEq, Eq)]
pub enum SyscallError {
    #[error("{0}: {1:?}")]
    InvalidString(Utf8Error, Vec<u8>),
    #[error("SBF program panicked")]
    Abort,
    #[error("SBF program Panicked in {0} at {1}:{2}")]
    Panic(String, u64, u64),
    #[error("Cannot borrow invoke context")]
    InvokeContextBorrowFailed,
    #[error("Malformed signer seed: {0}: {1:?}")]
    MalformedSignerSeed(Utf8Error, Vec<u8>),
    #[error("Could not create program address with signer seeds: {0}")]
    BadSeeds(PubkeyError),
    #[error("Program {0} not supported by inner instructions")]
    ProgramNotSupported(Pubkey),
    #[error("Unaligned pointer")]
    UnalignedPointer,
    #[error("Too many signers")]
    TooManySigners,
    #[error("Instruction passed to inner instruction is too large ({0} > {1})")]
    InstructionTooLarge(usize, usize),
    #[error("Too many accounts passed to inner instruction")]
    TooManyAccounts,
    #[error("Overlapping copy")]
    CopyOverlapping,
    #[error("Return data too large ({0} > {1})")]
    ReturnDataTooLarge(u64, u64),
    #[error("Hashing too many sequences")]
    TooManySlices,
    #[error("InvalidLength")]
    InvalidLength,
    #[error("Invoked an instruction with data that is too large ({data_len} > {max_data_len})")]
    MaxInstructionDataLenExceeded { data_len: u64, max_data_len: u64 },
    #[error("Invoked an instruction with too many accounts ({num_accounts} > {max_accounts})")]
    MaxInstructionAccountsExceeded {
        num_accounts: u64,
        max_accounts: u64,
    },
    #[error(
        "Invoked an instruction with too many account info's ({num_account_infos} > \
         {max_account_infos})"
    )]
    MaxInstructionAccountInfosExceeded {
        num_account_infos: u64,
        max_account_infos: u64,
    },
    #[error("InvalidAttribute")]
    InvalidAttribute,
    #[error("Invalid pointer")]
    InvalidPointer,
    #[error("Arithmetic overflow")]
    ArithmeticOverflow,
}

impl From<MemoryTranslationError> for SyscallError {
    fn from(error: MemoryTranslationError) -> Self {
        match error {
            MemoryTranslationError::UnalignedPointer => SyscallError::UnalignedPointer,
            MemoryTranslationError::InvalidLength => SyscallError::InvalidLength,
        }
    }
}

impl From<CpiError> for SyscallError {
    fn from(error: CpiError) -> Self {
        match error {
            CpiError::InvalidPointer => SyscallError::InvalidPointer,
            CpiError::TooManySigners => SyscallError::TooManySigners,
            CpiError::BadSeeds(e) => SyscallError::BadSeeds(e),
            CpiError::InvalidLength => SyscallError::InvalidLength,
            CpiError::MaxInstructionAccountsExceeded {
                num_accounts,
                max_accounts,
            } => SyscallError::MaxInstructionAccountsExceeded {
                num_accounts,
                max_accounts,
            },
            CpiError::MaxInstructionDataLenExceeded {
                data_len,
                max_data_len,
            } => SyscallError::MaxInstructionDataLenExceeded {
                data_len,
                max_data_len,
            },
            CpiError::MaxInstructionAccountInfosExceeded {
                num_account_infos,
                max_account_infos,
            } => SyscallError::MaxInstructionAccountInfosExceeded {
                num_account_infos,
                max_account_infos,
            },
            CpiError::ProgramNotSupported(pubkey) => SyscallError::ProgramNotSupported(pubkey),
        }
    }
}

type Error = Box<dyn std::error::Error>;

trait HasherImpl {
    const NAME: &'static str;
    type Output: AsRef<[u8]>;

    fn create_hasher() -> Self;
    fn hash(&mut self, val: &[u8]);
    fn result(self) -> Self::Output;
    fn get_base_cost(compute_cost: &SVMTransactionExecutionCost) -> u64;
    fn get_byte_cost(compute_cost: &SVMTransactionExecutionCost) -> u64;
    fn get_max_slices(compute_budget: &SVMTransactionExecutionBudget) -> u64;
}

struct Sha256Hasher(Hasher);
struct Blake3Hasher(blake3::Hasher);
struct Keccak256Hasher(keccak::Hasher);

impl HasherImpl for Sha256Hasher {
    const NAME: &'static str = "Sha256";
    type Output = Hash;

    fn create_hasher() -> Self {
        Sha256Hasher(Hasher::default())
    }

    fn hash(&mut self, val: &[u8]) {
        self.0.hash(val);
    }

    fn result(self) -> Self::Output {
        self.0.result()
    }

    fn get_base_cost(compute_cost: &SVMTransactionExecutionCost) -> u64 {
        compute_cost.sha256_base_cost
    }
    fn get_byte_cost(compute_cost: &SVMTransactionExecutionCost) -> u64 {
        compute_cost.sha256_byte_cost
    }
    fn get_max_slices(compute_budget: &SVMTransactionExecutionBudget) -> u64 {
        compute_budget.sha256_max_slices
    }
}

impl HasherImpl for Blake3Hasher {
    const NAME: &'static str = "Blake3";
    type Output = blake3::Hash;

    fn create_hasher() -> Self {
        Blake3Hasher(blake3::Hasher::default())
    }

    fn hash(&mut self, val: &[u8]) {
        self.0.hash(val);
    }

    fn result(self) -> Self::Output {
        self.0.result()
    }

    fn get_base_cost(compute_cost: &SVMTransactionExecutionCost) -> u64 {
        compute_cost.sha256_base_cost
    }
    fn get_byte_cost(compute_cost: &SVMTransactionExecutionCost) -> u64 {
        compute_cost.sha256_byte_cost
    }
    fn get_max_slices(compute_budget: &SVMTransactionExecutionBudget) -> u64 {
        compute_budget.sha256_max_slices
    }
}

impl HasherImpl for Keccak256Hasher {
    const NAME: &'static str = "Keccak256";
    type Output = keccak::Hash;

    fn create_hasher() -> Self {
        Keccak256Hasher(keccak::Hasher::default())
    }

    fn hash(&mut self, val: &[u8]) {
        self.0.hash(val);
    }

    fn result(self) -> Self::Output {
        self.0.result()
    }

    fn get_base_cost(compute_cost: &SVMTransactionExecutionCost) -> u64 {
        compute_cost.sha256_base_cost
    }
    fn get_byte_cost(compute_cost: &SVMTransactionExecutionCost) -> u64 {
        compute_cost.sha256_byte_cost
    }
    fn get_max_slices(compute_budget: &SVMTransactionExecutionBudget) -> u64 {
        compute_budget.sha256_max_slices
    }
}

fn consume_compute_meter(invoke_context: &InvokeContext, amount: u64) -> Result<(), Error> {
    invoke_context.consume_checked(amount)?;
    Ok(())
}

macro_rules! register_feature_gated_function {
    ($result:expr, $is_feature_active:expr, $name:expr, $call:expr $(,)?) => {
        if $is_feature_active {
            $result.register_function($name, $call)
        } else {
            Ok(())
        }
    };
}

pub fn create_program_runtime_environment_v1<'a, 'ix_data>(
    feature_set: &SVMFeatureSet,
    compute_budget: &SVMTransactionExecutionBudget,
    reject_deployment_of_broken_elfs: bool,
    debugging_features: bool,
) -> Result<BuiltinProgram<InvokeContext<'a, 'ix_data>>, Error> {
    let enable_alt_bn128_syscall = feature_set.enable_alt_bn128_syscall;
    let enable_alt_bn128_compression_syscall = feature_set.enable_alt_bn128_compression_syscall;
    let enable_big_mod_exp_syscall = feature_set.enable_big_mod_exp_syscall;
    let blake3_syscall_enabled = feature_set.blake3_syscall_enabled;
    let curve25519_syscall_enabled = feature_set.curve25519_syscall_enabled;
    let disable_fees_sysvar = feature_set.disable_fees_sysvar;
    let disable_deploy_of_alloc_free_syscall =
        reject_deployment_of_broken_elfs && feature_set.disable_deploy_of_alloc_free_syscall;
    let last_restart_slot_syscall_enabled = feature_set.last_restart_slot_sysvar;
    let enable_poseidon_syscall = feature_set.enable_poseidon_syscall;
    let remaining_compute_units_syscall_enabled =
        feature_set.remaining_compute_units_syscall_enabled;
    let get_sysvar_syscall_enabled = feature_set.get_sysvar_syscall_enabled;
    let enable_get_epoch_stake_syscall = feature_set.enable_get_epoch_stake_syscall;
    let min_sbpf_version =
        if !feature_set.disable_sbpf_v0_execution || feature_set.reenable_sbpf_v0_execution {
            SBPFVersion::V0
        } else {
            SBPFVersion::V3
        };
    let max_sbpf_version = if feature_set.enable_sbpf_v3_deployment_and_execution {
        SBPFVersion::V3
    } else if feature_set.enable_sbpf_v2_deployment_and_execution {
        SBPFVersion::V2
    } else if feature_set.enable_sbpf_v1_deployment_and_execution {
        SBPFVersion::V1
    } else {
        SBPFVersion::V0
    };
    debug_assert!(min_sbpf_version <= max_sbpf_version);

    let config = Config {
        max_call_depth: compute_budget.max_call_depth,
        stack_frame_size: compute_budget.stack_frame_size,
        enable_address_translation: true,
        enable_stack_frame_gaps: true,
        instruction_meter_checkpoint_distance: 10000,
        enable_instruction_meter: true,
        enable_register_tracing: debugging_features,
        enable_symbol_and_section_labels: debugging_features,
        reject_broken_elfs: reject_deployment_of_broken_elfs,
        noop_instruction_rate: 256,
        sanitize_user_provided_values: true,
        enabled_sbpf_versions: min_sbpf_version..=max_sbpf_version,
        optimize_rodata: false,
        aligned_memory_mapping: !feature_set.stricter_abi_and_runtime_constraints,
        // Warning, do not use `Config::default()` so that configuration here is explicit.
    };
    let mut result = BuiltinProgram::new_loader(config);

    // Abort
    result.register_function("abort", SyscallAbort::vm)?;

    // Panic
    result.register_function("sol_panic_", SyscallPanic::vm)?;

    // Logging
    result.register_function("sol_log_", SyscallLog::vm)?;
    result.register_function("sol_log_64_", SyscallLogU64::vm)?;
    result.register_function("sol_log_pubkey", SyscallLogPubkey::vm)?;
    result.register_function("sol_log_compute_units_", SyscallLogBpfComputeUnits::vm)?;

    // Program defined addresses (PDA)
    result.register_function(
        "sol_create_program_address",
        SyscallCreateProgramAddress::vm,
    )?;
    result.register_function(
        "sol_try_find_program_address",
        SyscallTryFindProgramAddress::vm,
    )?;

    // Sha256
    result.register_function("sol_sha256", SyscallHash::vm::<Sha256Hasher>)?;

    // Keccak256
    result.register_function("sol_keccak256", SyscallHash::vm::<Keccak256Hasher>)?;

    // Secp256k1 Recover
    result.register_function("sol_secp256k1_recover", SyscallSecp256k1Recover::vm)?;

    // Blake3
    register_feature_gated_function!(
        result,
        blake3_syscall_enabled,
        "sol_blake3",
        SyscallHash::vm::<Blake3Hasher>,
    )?;

    // Elliptic Curve Operations
    register_feature_gated_function!(
        result,
        curve25519_syscall_enabled,
        "sol_curve_validate_point",
        SyscallCurvePointValidation::vm,
    )?;
    register_feature_gated_function!(
        result,
        curve25519_syscall_enabled,
        "sol_curve_group_op",
        SyscallCurveGroupOps::vm,
    )?;
    register_feature_gated_function!(
        result,
        curve25519_syscall_enabled,
        "sol_curve_multiscalar_mul",
        SyscallCurveMultiscalarMultiplication::vm,
    )?;

    // Sysvars
    result.register_function("sol_get_clock_sysvar", SyscallGetClockSysvar::vm)?;
    result.register_function(
        "sol_get_epoch_schedule_sysvar",
        SyscallGetEpochScheduleSysvar::vm,
    )?;
    register_feature_gated_function!(
        result,
        !disable_fees_sysvar,
        "sol_get_fees_sysvar",
        SyscallGetFeesSysvar::vm,
    )?;
    result.register_function("sol_get_rent_sysvar", SyscallGetRentSysvar::vm)?;

    register_feature_gated_function!(
        result,
        last_restart_slot_syscall_enabled,
        "sol_get_last_restart_slot",
        SyscallGetLastRestartSlotSysvar::vm,
    )?;

    result.register_function(
        "sol_get_epoch_rewards_sysvar",
        SyscallGetEpochRewardsSysvar::vm,
    )?;

    // Memory ops
    result.register_function("sol_memcpy_", SyscallMemcpy::vm)?;
    result.register_function("sol_memmove_", SyscallMemmove::vm)?;
    result.register_function("sol_memset_", SyscallMemset::vm)?;
    result.register_function("sol_memcmp_", SyscallMemcmp::vm)?;

    // Processed sibling instructions
    result.register_function(
        "sol_get_processed_sibling_instruction",
        SyscallGetProcessedSiblingInstruction::vm,
    )?;

    // Stack height
    result.register_function("sol_get_stack_height", SyscallGetStackHeight::vm)?;

    // Return data
    result.register_function("sol_set_return_data", SyscallSetReturnData::vm)?;
    result.register_function("sol_get_return_data", SyscallGetReturnData::vm)?;

    // Cross-program invocation
    result.register_function("sol_invoke_signed_c", SyscallInvokeSignedC::vm)?;
    result.register_function("sol_invoke_signed_rust", SyscallInvokeSignedRust::vm)?;

    // Memory allocator
    register_feature_gated_function!(
        result,
        !disable_deploy_of_alloc_free_syscall,
        "sol_alloc_free_",
        SyscallAllocFree::vm,
    )?;

    // Alt_bn128
    register_feature_gated_function!(
        result,
        enable_alt_bn128_syscall,
        "sol_alt_bn128_group_op",
        SyscallAltBn128::vm,
    )?;

    // Big_mod_exp
    register_feature_gated_function!(
        result,
        enable_big_mod_exp_syscall,
        "sol_big_mod_exp",
        SyscallBigModExp::vm,
    )?;

    // Poseidon
    register_feature_gated_function!(
        result,
        enable_poseidon_syscall,
        "sol_poseidon",
        SyscallPoseidon::vm,
    )?;

    // Accessing remaining compute units
    register_feature_gated_function!(
        result,
        remaining_compute_units_syscall_enabled,
        "sol_remaining_compute_units",
        SyscallRemainingComputeUnits::vm
    )?;

    // Alt_bn128_compression
    register_feature_gated_function!(
        result,
        enable_alt_bn128_compression_syscall,
        "sol_alt_bn128_compression",
        SyscallAltBn128Compression::vm,
    )?;

    // Sysvar getter
    register_feature_gated_function!(
        result,
        get_sysvar_syscall_enabled,
        "sol_get_sysvar",
        SyscallGetSysvar::vm,
    )?;

    // Get Epoch Stake
    register_feature_gated_function!(
        result,
        enable_get_epoch_stake_syscall,
        "sol_get_epoch_stake",
        SyscallGetEpochStake::vm,
    )?;

    // Log data
    result.register_function("sol_log_data", SyscallLogData::vm)?;

    Ok(result)
}

pub fn create_program_runtime_environment_v2<'a, 'ix_data>(
    compute_budget: &SVMTransactionExecutionBudget,
    debugging_features: bool,
) -> BuiltinProgram<InvokeContext<'a, 'ix_data>> {
    let config = Config {
        max_call_depth: compute_budget.max_call_depth,
        stack_frame_size: compute_budget.stack_frame_size,
        enable_address_translation: true, // To be deactivated once we have BTF inference and verification
        enable_stack_frame_gaps: false,
        instruction_meter_checkpoint_distance: 10000,
        enable_instruction_meter: true,
        enable_register_tracing: debugging_features,
        enable_symbol_and_section_labels: debugging_features,
        reject_broken_elfs: true,
        noop_instruction_rate: 256,
        sanitize_user_provided_values: true,
        enabled_sbpf_versions: SBPFVersion::Reserved..=SBPFVersion::Reserved,
        optimize_rodata: true,
        aligned_memory_mapping: true,
        // Warning, do not use `Config::default()` so that configuration here is explicit.
    };
    BuiltinProgram::new_loader(config)
}

fn translate_type<'a, T>(
    memory_mapping: &'a MemoryMapping,
    vm_addr: u64,
    check_aligned: bool,
) -> Result<&'a T, Error> {
    translate_type_inner!(memory_mapping, AccessType::Load, vm_addr, T, check_aligned)
        .map(|value| &*value)
}
fn translate_slice<'a, T>(
    memory_mapping: &'a MemoryMapping,
    vm_addr: u64,
    len: u64,
    check_aligned: bool,
) -> Result<&'a [T], Error> {
    translate_slice_inner!(
        memory_mapping,
        AccessType::Load,
        vm_addr,
        len,
        T,
        check_aligned,
    )
    .map(|value| &*value)
}

/// Take a virtual pointer to a string (points to SBF VM memory space), translate it
/// pass it to a user-defined work function
fn translate_string_and_do(
    memory_mapping: &MemoryMapping,
    addr: u64,
    len: u64,
    check_aligned: bool,
    work: &mut dyn FnMut(&str) -> Result<u64, Error>,
) -> Result<u64, Error> {
    let buf = translate_slice::<u8>(memory_mapping, addr, len, check_aligned)?;
    match from_utf8(buf) {
        Ok(message) => work(message),
        Err(err) => Err(SyscallError::InvalidString(err, buf.to_vec()).into()),
    }
}

// Do not use this directly
#[allow(clippy::mut_from_ref)]
fn translate_type_mut<'a, T>(
    memory_mapping: &'a MemoryMapping,
    vm_addr: u64,
    check_aligned: bool,
) -> Result<&'a mut T, Error> {
    translate_type_inner!(memory_mapping, AccessType::Store, vm_addr, T, check_aligned)
}
// Do not use this directly
#[allow(clippy::mut_from_ref)]
fn translate_slice_mut<'a, T>(
    memory_mapping: &'a MemoryMapping,
    vm_addr: u64,
    len: u64,
    check_aligned: bool,
) -> Result<&'a mut [T], Error> {
    translate_slice_inner!(
        memory_mapping,
        AccessType::Store,
        vm_addr,
        len,
        T,
        check_aligned,
    )
}

fn touch_type_mut<T>(memory_mapping: &mut MemoryMapping, vm_addr: u64) -> Result<(), Error> {
    translate_inner!(
        memory_mapping,
        map_with_access_violation_handler,
        AccessType::Store,
        vm_addr,
        size_of::<T>() as u64,
    )
    .map(|_| ())
}
fn touch_slice_mut<T>(
    memory_mapping: &mut MemoryMapping,
    vm_addr: u64,
    element_count: u64,
) -> Result<(), Error> {
    if element_count == 0 {
        return Ok(());
    }
    translate_inner!(
        memory_mapping,
        map_with_access_violation_handler,
        AccessType::Store,
        vm_addr,
        element_count.saturating_mul(size_of::<T>() as u64),
    )
    .map(|_| ())
}

// No other translated references can be live when calling this.
// Meaning it should generally be at the beginning or end of a syscall and
// it should only be called once with all translations passed in one call.
#[macro_export]
macro_rules! translate_mut {
    (internal, $memory_mapping:expr, &mut [$T:ty], $vm_addr_and_element_count:expr) => {
        touch_slice_mut::<$T>(
            $memory_mapping,
            $vm_addr_and_element_count.0,
            $vm_addr_and_element_count.1,
        )?
    };
    (internal, $memory_mapping:expr, &mut $T:ty, $vm_addr:expr) => {
        touch_type_mut::<$T>(
            $memory_mapping,
            $vm_addr,
        )?
    };
    (internal, $memory_mapping:expr, $check_aligned:expr, &mut [$T:ty], $vm_addr_and_element_count:expr) => {{
        let slice = translate_slice_mut::<$T>(
            $memory_mapping,
            $vm_addr_and_element_count.0,
            $vm_addr_and_element_count.1,
            $check_aligned,
        )?;
        let host_addr = slice.as_ptr() as usize;
        (slice, host_addr, std::mem::size_of::<$T>().saturating_mul($vm_addr_and_element_count.1 as usize))
    }};
    (internal, $memory_mapping:expr, $check_aligned:expr, &mut $T:ty, $vm_addr:expr) => {{
        let reference = translate_type_mut::<$T>(
            $memory_mapping,
            $vm_addr,
            $check_aligned,
        )?;
        let host_addr = reference as *const _ as usize;
        (reference, host_addr, std::mem::size_of::<$T>())
    }};
    ($memory_mapping:expr, $check_aligned:expr, $(let $binding:ident : &mut $T:tt = map($vm_addr:expr $(, $element_count:expr)?) $try:tt;)+) => {
        // This ensures that all the parameters are collected first so that if they depend on previous translations
        $(let $binding = ($vm_addr $(, $element_count)?);)+
        // they are not invalidated by the following translations here:
        $(translate_mut!(internal, $memory_mapping, &mut $T, $binding);)+
        $(let $binding = translate_mut!(internal, $memory_mapping, $check_aligned, &mut $T, $binding);)+
        let host_ranges = [
            $(($binding.1, $binding.2),)+
        ];
        for (index, range_a) in host_ranges.get(..host_ranges.len().saturating_sub(1)).unwrap().iter().enumerate() {
            for range_b in host_ranges.get(index.saturating_add(1)..).unwrap().iter() {
                if !is_nonoverlapping(range_a.0, range_a.1, range_b.0, range_b.1) {
                    return Err(SyscallError::CopyOverlapping.into());
                }
            }
        }
        $(let $binding = $binding.0;)+
    };
}

declare_builtin_function!(
    /// Abort syscall functions, called when the SBF program calls `abort()`
    /// LLVM will insert calls to `abort()` if it detects an untenable situation,
    /// `abort()` is not intended to be called explicitly by the program.
    /// Causes the SBF program to be halted immediately
    SyscallAbort,
    fn rust(
        _invoke_context: &mut InvokeContext,
        _arg1: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        _memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        Err(SyscallError::Abort.into())
    }
);

declare_builtin_function!(
    /// Panic syscall function, called when the SBF program calls 'sol_panic_()`
    /// Causes the SBF program to be halted immediately
    SyscallPanic,
    fn rust(
        invoke_context: &mut InvokeContext,
        file: u64,
        len: u64,
        line: u64,
        column: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        consume_compute_meter(invoke_context, len)?;

        translate_string_and_do(
            memory_mapping,
            file,
            len,
            invoke_context.get_check_aligned(),
            &mut |string: &str| Err(SyscallError::Panic(string.to_string(), line, column).into()),
        )
    }
);

declare_builtin_function!(
    /// Dynamic memory allocation syscall called when the SBF program calls
    /// `sol_alloc_free_()`.  The allocator is expected to allocate/free
    /// from/to a given chunk of memory and enforce size restrictions.  The
    /// memory chunk is given to the allocator during allocator creation and
    /// information about that memory (start address and size) is passed
    /// to the VM to use for enforcement.
    SyscallAllocFree,
    fn rust(
        invoke_context: &mut InvokeContext,
        size: u64,
        free_addr: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        _memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let align = if invoke_context.get_check_aligned() {
            BPF_ALIGN_OF_U128
        } else {
            align_of::<u8>()
        };
        let Ok(layout) = Layout::from_size_align(size as usize, align) else {
            return Ok(0);
        };
        let allocator = &mut invoke_context.get_syscall_context_mut()?.allocator;
        if free_addr == 0 {
            match allocator.alloc(layout) {
                Ok(addr) => Ok(addr),
                Err(_) => Ok(0),
            }
        } else {
            // Unimplemented
            Ok(0)
        }
    }
);

fn translate_and_check_program_address_inputs<'a>(
    seeds_addr: u64,
    seeds_len: u64,
    program_id_addr: u64,
    memory_mapping: &'a mut MemoryMapping,
    check_aligned: bool,
) -> Result<(Vec<&'a [u8]>, &'a Pubkey), Error> {
    let untranslated_seeds =
        translate_slice::<VmSlice<u8>>(memory_mapping, seeds_addr, seeds_len, check_aligned)?;
    if untranslated_seeds.len() > MAX_SEEDS {
        return Err(SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded).into());
    }
    let seeds = untranslated_seeds
        .iter()
        .map(|untranslated_seed| {
            if untranslated_seed.len() > MAX_SEED_LEN as u64 {
                return Err(SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded).into());
            }
            translate_vm_slice(untranslated_seed, memory_mapping, check_aligned)
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let program_id = translate_type::<Pubkey>(memory_mapping, program_id_addr, check_aligned)?;
    Ok((seeds, program_id))
}

declare_builtin_function!(
    /// Create a program address
    SyscallCreateProgramAddress,
    fn rust(
        invoke_context: &mut InvokeContext,
        seeds_addr: u64,
        seeds_len: u64,
        program_id_addr: u64,
        address_addr: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let cost = invoke_context
            .get_execution_cost()
            .create_program_address_units;
        consume_compute_meter(invoke_context, cost)?;

        let (seeds, program_id) = translate_and_check_program_address_inputs(
            seeds_addr,
            seeds_len,
            program_id_addr,
            memory_mapping,
            invoke_context.get_check_aligned(),
        )?;

        let Ok(new_address) = Pubkey::create_program_address(&seeds, program_id) else {
            return Ok(1);
        };
        translate_mut!(
            memory_mapping,
            invoke_context.get_check_aligned(),
            let address: &mut [u8] = map(address_addr, std::mem::size_of::<Pubkey>() as u64)?;
        );
        address.copy_from_slice(new_address.as_ref());
        Ok(0)
    }
);

declare_builtin_function!(
    /// Create a program address
    SyscallTryFindProgramAddress,
    fn rust(
        invoke_context: &mut InvokeContext,
        seeds_addr: u64,
        seeds_len: u64,
        program_id_addr: u64,
        address_addr: u64,
        bump_seed_addr: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let cost = invoke_context
            .get_execution_cost()
            .create_program_address_units;
        consume_compute_meter(invoke_context, cost)?;

        let (seeds, program_id) = translate_and_check_program_address_inputs(
            seeds_addr,
            seeds_len,
            program_id_addr,
            memory_mapping,
            invoke_context.get_check_aligned(),
        )?;

        let mut bump_seed = [u8::MAX];
        for _ in 0..u8::MAX {
            {
                let mut seeds_with_bump = seeds.to_vec();
                seeds_with_bump.push(&bump_seed);

                if let Ok(new_address) =
                    Pubkey::create_program_address(&seeds_with_bump, program_id)
                {
                    translate_mut!(
                        memory_mapping,
                        invoke_context.get_check_aligned(),
                        let bump_seed_ref: &mut u8 = map(bump_seed_addr)?;
                        let address: &mut [u8] = map(address_addr, std::mem::size_of::<Pubkey>() as u64)?;
                    );
                    *bump_seed_ref = bump_seed[0];
                    address.copy_from_slice(new_address.as_ref());
                    return Ok(0);
                }
            }
            bump_seed[0] = bump_seed[0].saturating_sub(1);
            consume_compute_meter(invoke_context, cost)?;
        }
        Ok(1)
    }
);

declare_builtin_function!(
    /// secp256k1_recover
    SyscallSecp256k1Recover,
    fn rust(
        invoke_context: &mut InvokeContext,
        hash_addr: u64,
        recovery_id_val: u64,
        signature_addr: u64,
        result_addr: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let cost = invoke_context.get_execution_cost().secp256k1_recover_cost;
        consume_compute_meter(invoke_context, cost)?;

        translate_mut!(
            memory_mapping,
            invoke_context.get_check_aligned(),
            let secp256k1_recover_result: &mut [u8] = map(result_addr, SECP256K1_PUBLIC_KEY_LENGTH as u64)?;
        );
        let hash = translate_slice::<u8>(
            memory_mapping,
            hash_addr,
            keccak::HASH_BYTES as u64,
            invoke_context.get_check_aligned(),
        )?;
        let signature = translate_slice::<u8>(
            memory_mapping,
            signature_addr,
            SECP256K1_SIGNATURE_LENGTH as u64,
            invoke_context.get_check_aligned(),
        )?;

        let Ok(message) = libsecp256k1::Message::parse_slice(hash) else {
            return Ok(Secp256k1RecoverError::InvalidHash.into());
        };
        let Ok(adjusted_recover_id_val) = recovery_id_val.try_into() else {
            return Ok(Secp256k1RecoverError::InvalidRecoveryId.into());
        };
        let Ok(recovery_id) = libsecp256k1::RecoveryId::parse(adjusted_recover_id_val) else {
            return Ok(Secp256k1RecoverError::InvalidRecoveryId.into());
        };
        let Ok(signature) = libsecp256k1::Signature::parse_standard_slice(signature) else {
            return Ok(Secp256k1RecoverError::InvalidSignature.into());
        };

        let public_key = match libsecp256k1::recover(&message, &signature, &recovery_id) {
            Ok(key) => key.serialize(),
            Err(_) => {
                return Ok(Secp256k1RecoverError::InvalidSignature.into());
            }
        };

        secp256k1_recover_result.copy_from_slice(&public_key[1..65]);
        Ok(SUCCESS)
    }
);

declare_builtin_function!(
    // Elliptic Curve Point Validation
    //
    // Currently, only curve25519 Edwards and Ristretto representations are supported
    SyscallCurvePointValidation,
    fn rust(
        invoke_context: &mut InvokeContext,
        curve_id: u64,
        point_addr: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        use solana_curve25519::{curve_syscall_traits::*, edwards, ristretto};
        match curve_id {
            CURVE25519_EDWARDS => {
                let cost = invoke_context
                    .get_execution_cost()
                    .curve25519_edwards_validate_point_cost;
                consume_compute_meter(invoke_context, cost)?;

                let point = translate_type::<edwards::PodEdwardsPoint>(
                    memory_mapping,
                    point_addr,
                    invoke_context.get_check_aligned(),
                )?;

                if edwards::validate_edwards(point) {
                    Ok(0)
                } else {
                    Ok(1)
                }
            }
            CURVE25519_RISTRETTO => {
                let cost = invoke_context
                    .get_execution_cost()
                    .curve25519_ristretto_validate_point_cost;
                consume_compute_meter(invoke_context, cost)?;

                let point = translate_type::<ristretto::PodRistrettoPoint>(
                    memory_mapping,
                    point_addr,
                    invoke_context.get_check_aligned(),
                )?;

                if ristretto::validate_ristretto(point) {
                    Ok(0)
                } else {
                    Ok(1)
                }
            }
            _ => {
                if invoke_context.get_feature_set().abort_on_invalid_curve {
                    Err(SyscallError::InvalidAttribute.into())
                } else {
                    Ok(1)
                }
            }
        }
    }
);

declare_builtin_function!(
    // Elliptic Curve Group Operations
    //
    // Currently, only curve25519 Edwards and Ristretto representations are supported
    SyscallCurveGroupOps,
    fn rust(
        invoke_context: &mut InvokeContext,
        curve_id: u64,
        group_op: u64,
        left_input_addr: u64,
        right_input_addr: u64,
        result_point_addr: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        use solana_curve25519::{
            curve_syscall_traits::*,
            edwards::{self, PodEdwardsPoint},
            ristretto::{self, PodRistrettoPoint},
            scalar,
        };
        match curve_id {
            CURVE25519_EDWARDS => match group_op {
                ADD => {
                    let cost = invoke_context
                        .get_execution_cost()
                        .curve25519_edwards_add_cost;
                    consume_compute_meter(invoke_context, cost)?;

                    let left_point = translate_type::<PodEdwardsPoint>(
                        memory_mapping,
                        left_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;
                    let right_point = translate_type::<PodEdwardsPoint>(
                        memory_mapping,
                        right_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;

                    if let Some(result_point) = edwards::add_edwards(left_point, right_point) {
                        translate_mut!(
                            memory_mapping,
                            invoke_context.get_check_aligned(),
                            let result_point_ref_mut: &mut PodEdwardsPoint = map(result_point_addr)?;
                        );
                        *result_point_ref_mut = result_point;
                        Ok(0)
                    } else {
                        Ok(1)
                    }
                }
                SUB => {
                    let cost = invoke_context
                        .get_execution_cost()
                        .curve25519_edwards_subtract_cost;
                    consume_compute_meter(invoke_context, cost)?;

                    let left_point = translate_type::<PodEdwardsPoint>(
                        memory_mapping,
                        left_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;
                    let right_point = translate_type::<PodEdwardsPoint>(
                        memory_mapping,
                        right_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;

                    if let Some(result_point) = edwards::subtract_edwards(left_point, right_point) {
                        translate_mut!(
                            memory_mapping,
                            invoke_context.get_check_aligned(),
                            let result_point_ref_mut: &mut PodEdwardsPoint = map(result_point_addr)?;
                        );
                        *result_point_ref_mut = result_point;
                        Ok(0)
                    } else {
                        Ok(1)
                    }
                }
                MUL => {
                    let cost = invoke_context
                        .get_execution_cost()
                        .curve25519_edwards_multiply_cost;
                    consume_compute_meter(invoke_context, cost)?;

                    let scalar = translate_type::<scalar::PodScalar>(
                        memory_mapping,
                        left_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;
                    let input_point = translate_type::<PodEdwardsPoint>(
                        memory_mapping,
                        right_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;

                    if let Some(result_point) = edwards::multiply_edwards(scalar, input_point) {
                        translate_mut!(
                            memory_mapping,
                            invoke_context.get_check_aligned(),
                            let result_point_ref_mut: &mut PodEdwardsPoint = map(result_point_addr)?;
                        );
                        *result_point_ref_mut = result_point;
                        Ok(0)
                    } else {
                        Ok(1)
                    }
                }
                _ => {
                    if invoke_context.get_feature_set().abort_on_invalid_curve {
                        Err(SyscallError::InvalidAttribute.into())
                    } else {
                        Ok(1)
                    }
                }
            },

            CURVE25519_RISTRETTO => match group_op {
                ADD => {
                    let cost = invoke_context
                        .get_execution_cost()
                        .curve25519_ristretto_add_cost;
                    consume_compute_meter(invoke_context, cost)?;

                    let left_point = translate_type::<PodRistrettoPoint>(
                        memory_mapping,
                        left_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;
                    let right_point = translate_type::<PodRistrettoPoint>(
                        memory_mapping,
                        right_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;

                    if let Some(result_point) = ristretto::add_ristretto(left_point, right_point) {
                        translate_mut!(
                            memory_mapping,
                            invoke_context.get_check_aligned(),
                            let result_point_ref_mut: &mut PodRistrettoPoint = map(result_point_addr)?;
                        );
                        *result_point_ref_mut = result_point;
                        Ok(0)
                    } else {
                        Ok(1)
                    }
                }
                SUB => {
                    let cost = invoke_context
                        .get_execution_cost()
                        .curve25519_ristretto_subtract_cost;
                    consume_compute_meter(invoke_context, cost)?;

                    let left_point = translate_type::<PodRistrettoPoint>(
                        memory_mapping,
                        left_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;
                    let right_point = translate_type::<PodRistrettoPoint>(
                        memory_mapping,
                        right_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;

                    if let Some(result_point) =
                        ristretto::subtract_ristretto(left_point, right_point)
                    {
                        translate_mut!(
                            memory_mapping,
                            invoke_context.get_check_aligned(),
                            let result_point_ref_mut: &mut PodRistrettoPoint = map(result_point_addr)?;
                        );
                        *result_point_ref_mut = result_point;
                        Ok(0)
                    } else {
                        Ok(1)
                    }
                }
                MUL => {
                    let cost = invoke_context
                        .get_execution_cost()
                        .curve25519_ristretto_multiply_cost;
                    consume_compute_meter(invoke_context, cost)?;

                    let scalar = translate_type::<scalar::PodScalar>(
                        memory_mapping,
                        left_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;
                    let input_point = translate_type::<PodRistrettoPoint>(
                        memory_mapping,
                        right_input_addr,
                        invoke_context.get_check_aligned(),
                    )?;

                    if let Some(result_point) = ristretto::multiply_ristretto(scalar, input_point) {
                        translate_mut!(
                            memory_mapping,
                            invoke_context.get_check_aligned(),
                            let result_point_ref_mut: &mut PodRistrettoPoint = map(result_point_addr)?;
                        );
                        *result_point_ref_mut = result_point;
                        Ok(0)
                    } else {
                        Ok(1)
                    }
                }
                _ => {
                    if invoke_context.get_feature_set().abort_on_invalid_curve {
                        Err(SyscallError::InvalidAttribute.into())
                    } else {
                        Ok(1)
                    }
                }
            },

            _ => {
                if invoke_context.get_feature_set().abort_on_invalid_curve {
                    Err(SyscallError::InvalidAttribute.into())
                } else {
                    Ok(1)
                }
            }
        }
    }
);

declare_builtin_function!(
    // Elliptic Curve Multiscalar Multiplication
    //
    // Currently, only curve25519 Edwards and Ristretto representations are supported
    SyscallCurveMultiscalarMultiplication,
    fn rust(
        invoke_context: &mut InvokeContext,
        curve_id: u64,
        scalars_addr: u64,
        points_addr: u64,
        points_len: u64,
        result_point_addr: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        use solana_curve25519::{
            curve_syscall_traits::*,
            edwards::{self, PodEdwardsPoint},
            ristretto::{self, PodRistrettoPoint},
            scalar,
        };

        if points_len > 512 {
            return Err(Box::new(SyscallError::InvalidLength));
        }

        match curve_id {
            CURVE25519_EDWARDS => {
                let cost = invoke_context
                    .get_execution_cost()
                    .curve25519_edwards_msm_base_cost
                    .saturating_add(
                        invoke_context
                            .get_execution_cost()
                            .curve25519_edwards_msm_incremental_cost
                            .saturating_mul(points_len.saturating_sub(1)),
                    );
                consume_compute_meter(invoke_context, cost)?;

                let scalars = translate_slice::<scalar::PodScalar>(
                    memory_mapping,
                    scalars_addr,
                    points_len,
                    invoke_context.get_check_aligned(),
                )?;

                let points = translate_slice::<PodEdwardsPoint>(
                    memory_mapping,
                    points_addr,
                    points_len,
                    invoke_context.get_check_aligned(),
                )?;

                if let Some(result_point) = edwards::multiscalar_multiply_edwards(scalars, points) {
                    translate_mut!(
                        memory_mapping,
                        invoke_context.get_check_aligned(),
                        let result_point_ref_mut: &mut PodEdwardsPoint = map(result_point_addr)?;
                    );
                    *result_point_ref_mut = result_point;
                    Ok(0)
                } else {
                    Ok(1)
                }
            }

            CURVE25519_RISTRETTO => {
                let cost = invoke_context
                    .get_execution_cost()
                    .curve25519_ristretto_msm_base_cost
                    .saturating_add(
                        invoke_context
                            .get_execution_cost()
                            .curve25519_ristretto_msm_incremental_cost
                            .saturating_mul(points_len.saturating_sub(1)),
                    );
                consume_compute_meter(invoke_context, cost)?;

                let scalars = translate_slice::<scalar::PodScalar>(
                    memory_mapping,
                    scalars_addr,
                    points_len,
                    invoke_context.get_check_aligned(),
                )?;

                let points = translate_slice::<PodRistrettoPoint>(
                    memory_mapping,
                    points_addr,
                    points_len,
                    invoke_context.get_check_aligned(),
                )?;

                if let Some(result_point) =
                    ristretto::multiscalar_multiply_ristretto(scalars, points)
                {
                    translate_mut!(
                        memory_mapping,
                        invoke_context.get_check_aligned(),
                        let result_point_ref_mut: &mut PodRistrettoPoint = map(result_point_addr)?;
                    );
                    *result_point_ref_mut = result_point;
                    Ok(0)
                } else {
                    Ok(1)
                }
            }

            _ => {
                if invoke_context.get_feature_set().abort_on_invalid_curve {
                    Err(SyscallError::InvalidAttribute.into())
                } else {
                    Ok(1)
                }
            }
        }
    }
);

declare_builtin_function!(
    /// Set return data
    SyscallSetReturnData,
    fn rust(
        invoke_context: &mut InvokeContext,
        addr: u64,
        len: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let execution_cost = invoke_context.get_execution_cost();

        let cost = len
            .checked_div(execution_cost.cpi_bytes_per_unit)
            .unwrap_or(u64::MAX)
            .saturating_add(execution_cost.syscall_base_cost);
        consume_compute_meter(invoke_context, cost)?;

        if len > MAX_RETURN_DATA as u64 {
            return Err(SyscallError::ReturnDataTooLarge(len, MAX_RETURN_DATA as u64).into());
        }

        let return_data = if len == 0 {
            Vec::new()
        } else {
            translate_slice::<u8>(
                memory_mapping,
                addr,
                len,
                invoke_context.get_check_aligned(),
            )?
            .to_vec()
        };
        let transaction_context = &mut invoke_context.transaction_context;
        let program_id = *transaction_context
            .get_current_instruction_context()
            .and_then(|instruction_context| {
                instruction_context.get_program_key()
            })?;

        transaction_context.set_return_data(program_id, return_data)?;

        Ok(0)
    }
);

declare_builtin_function!(
    /// Get return data
    SyscallGetReturnData,
    fn rust(
        invoke_context: &mut InvokeContext,
        return_data_addr: u64,
        length: u64,
        program_id_addr: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let execution_cost = invoke_context.get_execution_cost();

        consume_compute_meter(invoke_context, execution_cost.syscall_base_cost)?;

        let (program_id, return_data) = invoke_context.transaction_context.get_return_data();
        let length = length.min(return_data.len() as u64);
        if length != 0 {
            let cost = length
                .saturating_add(size_of::<Pubkey>() as u64)
                .checked_div(execution_cost.cpi_bytes_per_unit)
                .unwrap_or(u64::MAX);
            consume_compute_meter(invoke_context, cost)?;

            translate_mut!(
                memory_mapping,
                invoke_context.get_check_aligned(),
                let return_data_result: &mut [u8] = map(return_data_addr, length)?;
                let program_id_result: &mut Pubkey = map(program_id_addr)?;
            );

            let to_slice = return_data_result;
            let from_slice = return_data
                .get(..length as usize)
                .ok_or(SyscallError::InvokeContextBorrowFailed)?;
            if to_slice.len() != from_slice.len() {
                return Err(SyscallError::InvalidLength.into());
            }
            to_slice.copy_from_slice(from_slice);
            *program_id_result = *program_id;
        }

        // Return the actual length, rather the length returned
        Ok(return_data.len() as u64)
    }
);

declare_builtin_function!(
    /// Get a processed sigling instruction
    SyscallGetProcessedSiblingInstruction,
    fn rust(
        invoke_context: &mut InvokeContext,
        index: u64,
        meta_addr: u64,
        program_id_addr: u64,
        data_addr: u64,
        accounts_addr: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let execution_cost = invoke_context.get_execution_cost();

        consume_compute_meter(invoke_context, execution_cost.syscall_base_cost)?;

        // Reverse iterate through the instruction trace,
        // ignoring anything except instructions on the same level
        let stack_height = invoke_context.get_stack_height();
        let instruction_trace_length = invoke_context
            .transaction_context
            .get_instruction_trace_length();
        let mut reverse_index_at_stack_height = 0;
        let mut found_instruction_context = None;
        for index_in_trace in (0..instruction_trace_length).rev() {
            let instruction_context = invoke_context
                .transaction_context
                .get_instruction_context_at_index_in_trace(index_in_trace)?;
            if instruction_context.get_stack_height() < stack_height {
                break;
            }
            if instruction_context.get_stack_height() == stack_height {
                if index.saturating_add(1) == reverse_index_at_stack_height {
                    found_instruction_context = Some(instruction_context);
                    break;
                }
                reverse_index_at_stack_height = reverse_index_at_stack_height.saturating_add(1);
            }
        }

        if let Some(instruction_context) = found_instruction_context {
            translate_mut!(
                memory_mapping,
                invoke_context.get_check_aligned(),
                let result_header: &mut ProcessedSiblingInstruction = map(meta_addr)?;
            );

            if result_header.data_len == (instruction_context.get_instruction_data().len() as u64)
                && result_header.accounts_len
                    == (instruction_context.get_number_of_instruction_accounts() as u64)
            {
                translate_mut!(
                    memory_mapping,
                    invoke_context.get_check_aligned(),
                    let program_id: &mut Pubkey = map(program_id_addr)?;
                    let data: &mut [u8] = map(data_addr, result_header.data_len)?;
                    let accounts: &mut [AccountMeta] = map(accounts_addr, result_header.accounts_len)?;
                    let result_header: &mut ProcessedSiblingInstruction = map(meta_addr)?;
                );
                // Marks result_header used. It had to be in translate_mut!() for the overlap checks.
                let _ = result_header;

                *program_id = *instruction_context
                    .get_program_key()?;
                data.clone_from_slice(instruction_context.get_instruction_data());
                let account_metas = (0..instruction_context.get_number_of_instruction_accounts())
                    .map(|instruction_account_index| {
                        Ok(AccountMeta {
                            pubkey: *instruction_context.get_key_of_instruction_account(instruction_account_index)?,
                            is_signer: instruction_context
                                .is_instruction_account_signer(instruction_account_index)?,
                            is_writable: instruction_context
                                .is_instruction_account_writable(instruction_account_index)?,
                        })
                    })
                    .collect::<Result<Vec<_>, InstructionError>>()?;
                accounts.clone_from_slice(account_metas.as_slice());
            } else {
                result_header.data_len = instruction_context.get_instruction_data().len() as u64;
                result_header.accounts_len =
                    instruction_context.get_number_of_instruction_accounts() as u64;
            }
            return Ok(true as u64);
        }
        Ok(false as u64)
    }
);

declare_builtin_function!(
    /// Get current call stack height
    SyscallGetStackHeight,
    fn rust(
        invoke_context: &mut InvokeContext,
        _arg1: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        _memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let execution_cost = invoke_context.get_execution_cost();

        consume_compute_meter(invoke_context, execution_cost.syscall_base_cost)?;

        Ok(invoke_context.get_stack_height() as u64)
    }
);

declare_builtin_function!(
    /// alt_bn128 group operations
    SyscallAltBn128,
    fn rust(
        invoke_context: &mut InvokeContext,
        group_op: u64,
        input_addr: u64,
        input_size: u64,
        result_addr: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        use solana_bn254::versioned::{
            alt_bn128_versioned_g1_addition, alt_bn128_versioned_g1_multiplication,
            alt_bn128_versioned_pairing, Endianness, VersionedG1Addition,
            VersionedG1Multiplication, VersionedPairing, ALT_BN128_ADDITION_OUTPUT_SIZE,
            ALT_BN128_G1_ADD_BE, ALT_BN128_G1_MUL_BE, ALT_BN128_MULTIPLICATION_OUTPUT_SIZE,
            ALT_BN128_PAIRING_BE, ALT_BN128_PAIRING_ELEMENT_SIZE,
            ALT_BN128_PAIRING_OUTPUT_SIZE, ALT_BN128_G1_ADD_LE, ALT_BN128_G1_MUL_LE,
            ALT_BN128_PAIRING_LE
        };

        let execution_cost = invoke_context.get_execution_cost();
        let (cost, output): (u64, usize) = match group_op {
            ALT_BN128_G1_ADD_BE | ALT_BN128_G1_ADD_LE => (
                execution_cost.alt_bn128_addition_cost,
                ALT_BN128_ADDITION_OUTPUT_SIZE,
            ),
            ALT_BN128_G1_MUL_BE | ALT_BN128_G1_MUL_LE => (
                execution_cost.alt_bn128_multiplication_cost,
                ALT_BN128_MULTIPLICATION_OUTPUT_SIZE,
            ),
            ALT_BN128_PAIRING_BE | ALT_BN128_PAIRING_LE => {
                let ele_len = input_size
                    .checked_div(ALT_BN128_PAIRING_ELEMENT_SIZE as u64)
                    .expect("div by non-zero constant");
                let cost = execution_cost
                    .alt_bn128_pairing_one_pair_cost_first
                    .saturating_add(
                        execution_cost
                            .alt_bn128_pairing_one_pair_cost_other
                            .saturating_mul(ele_len.saturating_sub(1)),
                    )
                    .saturating_add(execution_cost.sha256_base_cost)
                    .saturating_add(input_size)
                    .saturating_add(ALT_BN128_PAIRING_OUTPUT_SIZE as u64);
                (cost, ALT_BN128_PAIRING_OUTPUT_SIZE)
            }
            _ => {
                return Err(SyscallError::InvalidAttribute.into());
            }
        };

        consume_compute_meter(invoke_context, cost)?;

        translate_mut!(
            memory_mapping,
            invoke_context.get_check_aligned(),
            let call_result: &mut [u8] = map(result_addr, output as u64)?;
        );
        let input = translate_slice::<u8>(
            memory_mapping,
            input_addr,
            input_size,
            invoke_context.get_check_aligned(),
        )?;

        let result_point = match group_op {
            ALT_BN128_G1_ADD_BE => {
                alt_bn128_versioned_g1_addition(VersionedG1Addition::V0, input, Endianness::BE)
            }
            ALT_BN128_G1_ADD_LE => {
                if invoke_context.get_feature_set().alt_bn128_little_endian {
                    alt_bn128_versioned_g1_addition(VersionedG1Addition::V0, input, Endianness::LE)
                } else {
                    return Err(SyscallError::InvalidAttribute.into());
                }
            }
            ALT_BN128_G1_MUL_BE => {
                alt_bn128_versioned_g1_multiplication(
                    VersionedG1Multiplication::V1,
                    input,
                    Endianness::BE
                )
            }
            ALT_BN128_G1_MUL_LE => {
                if invoke_context.get_feature_set().alt_bn128_little_endian {
                    alt_bn128_versioned_g1_multiplication(
                        VersionedG1Multiplication::V1,
                        input,
                        Endianness::LE
                    )
                } else {
                    return Err(SyscallError::InvalidAttribute.into());
                }
            }
            ALT_BN128_PAIRING_BE => {
                let version = if invoke_context
                    .get_feature_set()
                    .fix_alt_bn128_pairing_length_check {
                    VersionedPairing::V1
                } else {
                    VersionedPairing::V0
                };
                alt_bn128_versioned_pairing(version, input, Endianness::BE)
            }
            ALT_BN128_PAIRING_LE => {
                if invoke_context.get_feature_set().alt_bn128_little_endian {
                    alt_bn128_versioned_pairing(VersionedPairing::V1, input, Endianness::LE)
                } else {
                    return Err(SyscallError::InvalidAttribute.into());
                }
            }
            _ => {
                return Err(SyscallError::InvalidAttribute.into());
            }
        };

        match result_point {
            Ok(point) => {
                call_result.copy_from_slice(&point);
                Ok(SUCCESS)
            }
            Err(_) => {
                Ok(1)
            }
        }
    }
);

declare_builtin_function!(
    /// Big integer modular exponentiation
    SyscallBigModExp,
    fn rust(
        invoke_context: &mut InvokeContext,
        params: u64,
        return_value: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let params = &translate_slice::<BigModExpParams>(
            memory_mapping,
            params,
            1,
            invoke_context.get_check_aligned(),
        )?
        .first()
        .ok_or(SyscallError::InvalidLength)?;

        if params.base_len > 512 || params.exponent_len > 512 || params.modulus_len > 512 {
            return Err(Box::new(SyscallError::InvalidLength));
        }

        let input_len: u64 = std::cmp::max(params.base_len, params.exponent_len);
        let input_len: u64 = std::cmp::max(input_len, params.modulus_len);

        let execution_cost = invoke_context.get_execution_cost();
        // the compute units are calculated by the quadratic equation `0.5 input_len^2 + 190`
        consume_compute_meter(
            invoke_context,
            execution_cost.syscall_base_cost.saturating_add(
                input_len
                    .saturating_mul(input_len)
                    .checked_div(execution_cost.big_modular_exponentiation_cost_divisor)
                    .unwrap_or(u64::MAX)
                    .saturating_add(execution_cost.big_modular_exponentiation_base_cost),
            ),
        )?;

        let base = translate_slice::<u8>(
            memory_mapping,
            params.base as *const _ as u64,
            params.base_len,
            invoke_context.get_check_aligned(),
        )?;

        let exponent = translate_slice::<u8>(
            memory_mapping,
            params.exponent as *const _ as u64,
            params.exponent_len,
            invoke_context.get_check_aligned(),
        )?;

        let modulus = translate_slice::<u8>(
            memory_mapping,
            params.modulus as *const _ as u64,
            params.modulus_len,
            invoke_context.get_check_aligned(),
        )?;

        let value = big_mod_exp(base, exponent, modulus);

        translate_mut!(
            memory_mapping,
            invoke_context.get_check_aligned(),
            let return_value_ref_mut: &mut [u8] = map(return_value, params.modulus_len)?;
        );
        return_value_ref_mut.copy_from_slice(value.as_slice());

        Ok(0)
    }
);

declare_builtin_function!(
    // Poseidon
    SyscallPoseidon,
    fn rust(
        invoke_context: &mut InvokeContext,
        parameters: u64,
        endianness: u64,
        vals_addr: u64,
        vals_len: u64,
        result_addr: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let parameters: poseidon::Parameters = parameters.try_into()?;
        let endianness: poseidon::Endianness = endianness.try_into()?;

        if vals_len > 12 {
            ic_msg!(
                invoke_context,
                "Poseidon hashing {} sequences is not supported",
                vals_len,
            );
            return Err(SyscallError::InvalidLength.into());
        }

        let execution_cost = invoke_context.get_execution_cost();
        let Some(cost) = execution_cost.poseidon_cost(vals_len) else {
            ic_msg!(
                invoke_context,
                "Overflow while calculating the compute cost"
            );
            return Err(SyscallError::ArithmeticOverflow.into());
        };
        consume_compute_meter(invoke_context, cost.to_owned())?;

        translate_mut!(
            memory_mapping,
            invoke_context.get_check_aligned(),
            let hash_result: &mut [u8] = map(result_addr, poseidon::HASH_BYTES as u64)?;
        );
        let inputs = translate_slice::<VmSlice<u8>>(
            memory_mapping,
            vals_addr,
            vals_len,
            invoke_context.get_check_aligned(),
        )?;
        let inputs = inputs
            .iter()
            .map(|input| {
                translate_vm_slice(input, memory_mapping, invoke_context.get_check_aligned())
            })
            .collect::<Result<Vec<_>, Error>>()?;

        let result = if invoke_context.get_feature_set().poseidon_enforce_padding {
            poseidon::hashv(parameters, endianness, inputs.as_slice())
        } else {
            poseidon::legacy::hashv(parameters, endianness, inputs.as_slice())
        };
        let Ok(hash) = result else {
            return Ok(1);
        };
        hash_result.copy_from_slice(&hash.to_bytes());

        Ok(SUCCESS)
    }
);

declare_builtin_function!(
    /// Read remaining compute units
    SyscallRemainingComputeUnits,
    fn rust(
        invoke_context: &mut InvokeContext,
        _arg1: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        _memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let execution_cost = invoke_context.get_execution_cost();
        consume_compute_meter(invoke_context, execution_cost.syscall_base_cost)?;

        use solana_sbpf::vm::ContextObject;
        Ok(invoke_context.get_remaining())
    }
);

declare_builtin_function!(
    /// alt_bn128 g1 and g2 compression and decompression
    SyscallAltBn128Compression,
    fn rust(
        invoke_context: &mut InvokeContext,
        op: u64,
        input_addr: u64,
        input_size: u64,
        result_addr: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        use solana_bn254::{
            prelude::{ALT_BN128_G1_POINT_SIZE, ALT_BN128_G2_POINT_SIZE},
            compression::prelude::{
                alt_bn128_g1_compress, alt_bn128_g1_decompress,
                alt_bn128_g2_compress, alt_bn128_g2_decompress,
                alt_bn128_g1_compress_le, alt_bn128_g1_decompress_le,
                alt_bn128_g2_compress_le, alt_bn128_g2_decompress_le,
                ALT_BN128_G1_COMPRESS_BE, ALT_BN128_G1_DECOMPRESS_BE,
                ALT_BN128_G2_COMPRESS_BE, ALT_BN128_G2_DECOMPRESS_BE,
                ALT_BN128_G1_COMPRESSED_POINT_SIZE, ALT_BN128_G2_COMPRESSED_POINT_SIZE,
                ALT_BN128_G1_COMPRESS_LE, ALT_BN128_G2_COMPRESS_LE,
                ALT_BN128_G1_DECOMPRESS_LE, ALT_BN128_G2_DECOMPRESS_LE,
            }
        };
        let execution_cost = invoke_context.get_execution_cost();
        let base_cost = execution_cost.syscall_base_cost;
        let (cost, output): (u64, usize) = match op {
            ALT_BN128_G1_COMPRESS_BE | ALT_BN128_G1_COMPRESS_LE => (
                base_cost.saturating_add(execution_cost.alt_bn128_g1_compress),
                ALT_BN128_G1_COMPRESSED_POINT_SIZE,
            ),
            ALT_BN128_G1_DECOMPRESS_BE | ALT_BN128_G1_DECOMPRESS_LE => {
                (base_cost.saturating_add(execution_cost.alt_bn128_g1_decompress), ALT_BN128_G1_POINT_SIZE)
            }
            ALT_BN128_G2_COMPRESS_BE | ALT_BN128_G2_COMPRESS_LE => (
                base_cost.saturating_add(execution_cost.alt_bn128_g2_compress),
                ALT_BN128_G2_COMPRESSED_POINT_SIZE,
            ),
            ALT_BN128_G2_DECOMPRESS_BE | ALT_BN128_G2_DECOMPRESS_LE => {
                (base_cost.saturating_add(execution_cost.alt_bn128_g2_decompress), ALT_BN128_G2_POINT_SIZE)
            }
            _ => {
                return Err(SyscallError::InvalidAttribute.into());
            }
        };

        consume_compute_meter(invoke_context, cost)?;

        translate_mut!(
            memory_mapping,
            invoke_context.get_check_aligned(),
            let call_result: &mut [u8] = map(result_addr, output as u64)?;
        );
        let input = translate_slice::<u8>(
            memory_mapping,
            input_addr,
            input_size,
            invoke_context.get_check_aligned(),
        )?;

        match op {
            ALT_BN128_G1_COMPRESS_BE => {
                let Ok(result_point) = alt_bn128_g1_compress(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G1_COMPRESS_LE => {
                if invoke_context.get_feature_set().alt_bn128_little_endian {
                    let Ok(result_point) = alt_bn128_g1_compress_le(input) else {
                        return Ok(1);
                    };
                    call_result.copy_from_slice(&result_point);
                } else {
                    return Err(SyscallError::InvalidAttribute.into());
                }
            }
            ALT_BN128_G1_DECOMPRESS_BE => {
                let Ok(result_point) = alt_bn128_g1_decompress(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G1_DECOMPRESS_LE => {
                if invoke_context.get_feature_set().alt_bn128_little_endian {
                    let Ok(result_point) = alt_bn128_g1_decompress_le(input) else {
                        return Ok(1);
                    };
                    call_result.copy_from_slice(&result_point);
                } else {
                    return Err(SyscallError::InvalidAttribute.into());
                }
            }
            ALT_BN128_G2_COMPRESS_BE => {
                let Ok(result_point) = alt_bn128_g2_compress(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G2_COMPRESS_LE => {
                if invoke_context.get_feature_set().alt_bn128_little_endian {
                    let Ok(result_point) = alt_bn128_g2_compress_le(input) else {
                        return Ok(1);
                    };
                    call_result.copy_from_slice(&result_point);
                } else {
                    return Err(SyscallError::InvalidAttribute.into());
                }
            }
            ALT_BN128_G2_DECOMPRESS_BE => {
                let Ok(result_point) = alt_bn128_g2_decompress(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G2_DECOMPRESS_LE => {
                if invoke_context.get_feature_set().alt_bn128_little_endian {
                    let Ok(result_point) = alt_bn128_g2_decompress_le(input) else {
                        return Ok(1);
                    };
                    call_result.copy_from_slice(&result_point);
                } else {
                    return Err(SyscallError::InvalidAttribute.into());
                }
            }
            _ => return Err(SyscallError::InvalidAttribute.into()),
        }

        Ok(SUCCESS)
    }
);

declare_builtin_function!(
    // Generic Hashing Syscall
    SyscallHash<H: HasherImpl>,
    fn rust(
        invoke_context: &mut InvokeContext,
        vals_addr: u64,
        vals_len: u64,
        result_addr: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let compute_budget = invoke_context.get_compute_budget();
        let compute_cost = invoke_context.get_execution_cost();
        let hash_base_cost = H::get_base_cost(compute_cost);
        let hash_byte_cost = H::get_byte_cost(compute_cost);
        let hash_max_slices = H::get_max_slices(compute_budget);
        if hash_max_slices < vals_len {
            ic_msg!(
                invoke_context,
                "{} Hashing {} sequences in one syscall is over the limit {}",
                H::NAME,
                vals_len,
                hash_max_slices,
            );
            return Err(SyscallError::TooManySlices.into());
        }

        consume_compute_meter(invoke_context, hash_base_cost)?;

        translate_mut!(
            memory_mapping,
            invoke_context.get_check_aligned(),
            let hash_result: &mut [u8] = map(result_addr, std::mem::size_of::<H::Output>() as u64)?;
        );
        let mut hasher = H::create_hasher();
        if vals_len > 0 {
            let vals = translate_slice::<VmSlice<u8>>(
                memory_mapping,
                vals_addr,
                vals_len,
                invoke_context.get_check_aligned(),
            )?;

            for val in vals.iter() {
                let bytes = translate_vm_slice(val, memory_mapping, invoke_context.get_check_aligned())?;
                let cost = compute_cost.mem_op_base_cost.max(
                    hash_byte_cost.saturating_mul(
                        val.len()
                            .checked_div(2)
                            .expect("div by non-zero literal"),
                    ),
                );
                consume_compute_meter(invoke_context, cost)?;
                hasher.hash(bytes);
            }
        }
        hash_result.copy_from_slice(hasher.result().as_ref());
        Ok(0)
    }
);

declare_builtin_function!(
    // Get Epoch Stake Syscall
    SyscallGetEpochStake,
    fn rust(
        invoke_context: &mut InvokeContext,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        let compute_cost = invoke_context.get_execution_cost();

        if var_addr == 0 {
            // As specified by SIMD-0133: If `var_addr` is a null pointer:
            //
            // Compute units:
            //
            // ```
            // syscall_base
            // ```
            let compute_units = compute_cost.syscall_base_cost;
            consume_compute_meter(invoke_context, compute_units)?;
            //
            // Control flow:
            //
            // - The syscall aborts the virtual machine if:
            //     - Compute budget is exceeded.
            // - Otherwise, the syscall returns a `u64` integer representing the total active
            //   stake on the cluster for the current epoch.
            Ok(invoke_context.get_epoch_stake())
        } else {
            // As specified by SIMD-0133: If `var_addr` is _not_ a null pointer:
            //
            // Compute units:
            //
            // ```
            // syscall_base + floor(PUBKEY_BYTES/cpi_bytes_per_unit) + mem_op_base
            // ```
            let compute_units = compute_cost
                .syscall_base_cost
                .saturating_add(
                    (PUBKEY_BYTES as u64)
                        .checked_div(compute_cost.cpi_bytes_per_unit)
                        .unwrap_or(u64::MAX),
                )
                .saturating_add(compute_cost.mem_op_base_cost);
            consume_compute_meter(invoke_context, compute_units)?;
            //
            // Control flow:
            //
            // - The syscall aborts the virtual machine if:
            //     - Not all bytes in VM memory range `[vote_addr, vote_addr + 32)` are
            //       readable.
            //     - Compute budget is exceeded.
            // - Otherwise, the syscall returns a `u64` integer representing the total active
            //   stake delegated to the vote account at the provided address.
            //   If the provided vote address corresponds to an account that is not a vote
            //   account or does not exist, the syscall will return `0` for active stake.
            let check_aligned = invoke_context.get_check_aligned();
            let vote_address = translate_type::<Pubkey>(memory_mapping, var_addr, check_aligned)?;

            Ok(invoke_context.get_epoch_stake_for_vote_account(vote_address))
        }
    }
);

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
#[allow(clippy::indexing_slicing)]
mod tests {
    #[allow(deprecated)]
    use solana_sysvar::fees::Fees;
    use {
        super::*,
        assert_matches::assert_matches,
        core::slice,
        solana_account::{create_account_shared_data_for_test, AccountSharedData},
        solana_account_info::AccountInfo,
        solana_clock::Clock,
        solana_epoch_rewards::EpochRewards,
        solana_epoch_schedule::EpochSchedule,
        solana_fee_calculator::FeeCalculator,
        solana_hash::HASH_BYTES,
        solana_instruction::Instruction,
        solana_last_restart_slot::LastRestartSlot,
        solana_program::program::check_type_assumptions,
        solana_program_runtime::{
            execution_budget::MAX_HEAP_FRAME_BYTES,
            invoke_context::{BpfAllocator, InvokeContext, SyscallContext},
            memory::address_is_aligned,
            with_mock_invoke_context,
        },
        solana_sbpf::{
            aligned_memory::AlignedMemory,
            ebpf::{self, HOST_ALIGN},
            error::EbpfError,
            memory_region::{MemoryMapping, MemoryRegion},
            program::SBPFVersion,
            vm::Config,
        },
        solana_sdk_ids::{
            bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, native_loader, sysvar,
        },
        solana_sha256_hasher::hashv,
        solana_slot_hashes::{self as slot_hashes, SlotHashes},
        solana_stable_layout::stable_instruction::StableInstruction,
        solana_stake_interface::stake_history::{self, StakeHistory, StakeHistoryEntry},
        solana_sysvar_id::SysvarId,
        solana_transaction_context::{instruction_accounts::InstructionAccount, IndexOfAccount},
        std::{
            hash::{DefaultHasher, Hash, Hasher},
            mem,
            str::FromStr,
        },
        test_case::test_case,
    };

    macro_rules! assert_access_violation {
        ($result:expr, $va:expr, $len:expr) => {
            match $result.unwrap_err().downcast_ref::<EbpfError>().unwrap() {
                EbpfError::AccessViolation(_, va, len, _) if $va == *va && $len == *len => {}
                EbpfError::StackAccessViolation(_, va, len, _) if $va == *va && $len == *len => {}
                _ => panic!(),
            }
        };
    }

    macro_rules! prepare_mockup {
        ($invoke_context:ident,
         $program_key:ident,
         $loader_key:expr $(,)?) => {
            let $program_key = Pubkey::new_unique();
            let transaction_accounts = vec![
                (
                    $loader_key,
                    AccountSharedData::new(0, 0, &native_loader::id()),
                ),
                ($program_key, AccountSharedData::new(0, 0, &$loader_key)),
            ];
            with_mock_invoke_context!($invoke_context, transaction_context, transaction_accounts);
            $invoke_context
                .transaction_context
                .configure_next_instruction_for_tests(1, vec![], vec![])
                .unwrap();
            $invoke_context.push().unwrap();
        };
    }

    #[allow(dead_code)]
    struct MockSlice {
        vm_addr: u64,
        len: usize,
    }

    #[test]
    fn test_translate() {
        const START: u64 = 0x100000000;
        const LENGTH: u64 = 1000;

        let data = vec![0u8; LENGTH as usize];
        let addr = data.as_ptr() as u64;
        let config = Config::default();
        let memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(&data, START)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let cases = vec![
            (true, START, 0, addr),
            (true, START, 1, addr),
            (true, START, LENGTH, addr),
            (true, START + 1, LENGTH - 1, addr + 1),
            (false, START + 1, LENGTH, 0),
            (true, START + LENGTH - 1, 1, addr + LENGTH - 1),
            (true, START + LENGTH, 0, addr + LENGTH),
            (false, START + LENGTH, 1, 0),
            (false, START, LENGTH + 1, 0),
            (false, 0, 0, 0),
            (false, 0, 1, 0),
            (false, START - 1, 0, 0),
            (false, START - 1, 1, 0),
            (true, START + LENGTH / 2, LENGTH / 2, addr + LENGTH / 2),
        ];
        for (ok, start, length, value) in cases {
            if ok {
                assert_eq!(
                    translate_inner!(&memory_mapping, map, AccessType::Load, start, length)
                        .unwrap(),
                    value
                )
            } else {
                assert!(
                    translate_inner!(&memory_mapping, map, AccessType::Load, start, length)
                        .is_err()
                )
            }
        }
    }

    #[test]
    fn test_translate_type() {
        let config = Config::default();

        // Pubkey
        let pubkey = solana_pubkey::new_rand();
        let memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(bytes_of(&pubkey), 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        let translated_pubkey =
            translate_type::<Pubkey>(&memory_mapping, 0x100000000, true).unwrap();
        assert_eq!(pubkey, *translated_pubkey);

        // Instruction
        let instruction = Instruction::new_with_bincode(
            solana_pubkey::new_rand(),
            &"foobar",
            vec![AccountMeta::new(solana_pubkey::new_rand(), false)],
        );
        let instruction = StableInstruction::from(instruction);
        let memory_region = MemoryRegion::new_readonly(bytes_of(&instruction), 0x100000000);
        let memory_mapping =
            MemoryMapping::new(vec![memory_region], &config, SBPFVersion::V3).unwrap();
        let translated_instruction =
            translate_type::<StableInstruction>(&memory_mapping, 0x100000000, true).unwrap();
        assert_eq!(instruction, *translated_instruction);

        let memory_region = MemoryRegion::new_readonly(&bytes_of(&instruction)[..1], 0x100000000);
        let memory_mapping =
            MemoryMapping::new(vec![memory_region], &config, SBPFVersion::V3).unwrap();
        assert!(translate_type::<Instruction>(&memory_mapping, 0x100000000, true).is_err());
    }

    #[test]
    fn test_translate_slice() {
        let config = Config::default();

        // zero len
        let good_data = vec![1u8, 2, 3, 4, 5];
        let data: Vec<u8> = vec![];
        assert_eq!(std::ptr::dangling::<u8>(), data.as_ptr());
        let memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(&good_data, 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        let translated_data =
            translate_slice::<u8>(&memory_mapping, data.as_ptr() as u64, 0, true).unwrap();
        assert_eq!(data, translated_data);
        assert_eq!(0, translated_data.len());

        // u8
        let mut data = vec![1u8, 2, 3, 4, 5];
        let memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(&data, 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        let translated_data =
            translate_slice::<u8>(&memory_mapping, 0x100000000, data.len() as u64, true).unwrap();
        assert_eq!(data, translated_data);
        *data.first_mut().unwrap() = 10;
        assert_eq!(data, translated_data);
        assert!(
            translate_slice::<u8>(&memory_mapping, data.as_ptr() as u64, u64::MAX, true).is_err()
        );

        assert!(
            translate_slice::<u8>(&memory_mapping, 0x100000000 - 1, data.len() as u64, true,)
                .is_err()
        );

        // u64
        let mut data = vec![1u64, 2, 3, 4, 5];
        let memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(
                bytes_of_slice(&data),
                0x100000000,
            )],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        let translated_data =
            translate_slice::<u64>(&memory_mapping, 0x100000000, data.len() as u64, true).unwrap();
        assert_eq!(data, translated_data);
        *data.first_mut().unwrap() = 10;
        assert_eq!(data, translated_data);
        assert!(translate_slice::<u64>(&memory_mapping, 0x100000000, u64::MAX, true).is_err());

        // Pubkeys
        let mut data = vec![solana_pubkey::new_rand(); 5];
        let memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(
                unsafe {
                    slice::from_raw_parts(data.as_ptr() as *const u8, mem::size_of::<Pubkey>() * 5)
                },
                0x100000000,
            )],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        let translated_data =
            translate_slice::<Pubkey>(&memory_mapping, 0x100000000, data.len() as u64, true)
                .unwrap();
        assert_eq!(data, translated_data);
        *data.first_mut().unwrap() = solana_pubkey::new_rand(); // Both should point to same place
        assert_eq!(data, translated_data);
    }

    #[test]
    fn test_translate_string_and_do() {
        let string = "Gaggablaghblagh!";
        let config = Config::default();
        let memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(string.as_bytes(), 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        assert_eq!(
            42,
            translate_string_and_do(
                &memory_mapping,
                0x100000000,
                string.len() as u64,
                true,
                &mut |string: &str| {
                    assert_eq!(string, "Gaggablaghblagh!");
                    Ok(42)
                }
            )
            .unwrap()
        );
    }

    #[test]
    #[should_panic(expected = "Abort")]
    fn test_syscall_abort() {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(vec![], &config, SBPFVersion::V3).unwrap();
        let result = SyscallAbort::rust(&mut invoke_context, 0, 0, 0, 0, 0, &mut memory_mapping);
        result.unwrap();
    }

    #[test]
    #[should_panic(expected = "Panic(\"Gaggablaghblagh!\", 42, 84)")]
    fn test_syscall_sol_panic() {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        let string = "Gaggablaghblagh!";
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(string.as_bytes(), 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        invoke_context.mock_set_remaining(string.len() as u64 - 1);
        let result = SyscallPanic::rust(
            &mut invoke_context,
            0x100000000,
            string.len() as u64,
            42,
            84,
            0,
            &mut memory_mapping,
        );
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );

        invoke_context.mock_set_remaining(string.len() as u64);
        let result = SyscallPanic::rust(
            &mut invoke_context,
            0x100000000,
            string.len() as u64,
            42,
            84,
            0,
            &mut memory_mapping,
        );
        result.unwrap();
    }

    #[test]
    fn test_syscall_sol_log() {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        let string = "Gaggablaghblagh!";
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(string.as_bytes(), 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        invoke_context.mock_set_remaining(400 - 1);
        let result = SyscallLog::rust(
            &mut invoke_context,
            0x100000001, // AccessViolation
            string.len() as u64,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_access_violation!(result, 0x100000001, string.len() as u64);
        let result = SyscallLog::rust(
            &mut invoke_context,
            0x100000000,
            string.len() as u64 * 2, // AccessViolation
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_access_violation!(result, 0x100000000, string.len() as u64 * 2);

        let result = SyscallLog::rust(
            &mut invoke_context,
            0x100000000,
            string.len() as u64,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        result.unwrap();
        let result = SyscallLog::rust(
            &mut invoke_context,
            0x100000000,
            string.len() as u64,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );

        assert_eq!(
            invoke_context
                .get_log_collector()
                .unwrap()
                .borrow()
                .get_recorded_content(),
            &["Program log: Gaggablaghblagh!".to_string()]
        );
    }

    #[test]
    fn test_syscall_sol_log_u64() {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let cost = invoke_context.get_execution_cost().log_64_units;

        invoke_context.mock_set_remaining(cost);
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(vec![], &config, SBPFVersion::V3).unwrap();
        let result = SyscallLogU64::rust(&mut invoke_context, 1, 2, 3, 4, 5, &mut memory_mapping);
        result.unwrap();

        assert_eq!(
            invoke_context
                .get_log_collector()
                .unwrap()
                .borrow()
                .get_recorded_content(),
            &["Program log: 0x1, 0x2, 0x3, 0x4, 0x5".to_string()]
        );
    }

    #[test]
    fn test_syscall_sol_pubkey() {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let cost = invoke_context.get_execution_cost().log_pubkey_units;

        let pubkey = Pubkey::from_str("MoqiU1vryuCGQSxFKA1SZ316JdLEFFhoAu6cKUNk7dN").unwrap();
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(bytes_of(&pubkey), 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let result = SyscallLogPubkey::rust(
            &mut invoke_context,
            0x100000001, // AccessViolation
            32,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_access_violation!(result, 0x100000001, 32);

        invoke_context.mock_set_remaining(1);
        let result =
            SyscallLogPubkey::rust(&mut invoke_context, 100, 32, 0, 0, 0, &mut memory_mapping);
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );

        invoke_context.mock_set_remaining(cost);
        let result = SyscallLogPubkey::rust(
            &mut invoke_context,
            0x100000000,
            0,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        result.unwrap();

        assert_eq!(
            invoke_context
                .get_log_collector()
                .unwrap()
                .borrow()
                .get_recorded_content(),
            &["Program log: MoqiU1vryuCGQSxFKA1SZ316JdLEFFhoAu6cKUNk7dN".to_string()]
        );
    }

    macro_rules! setup_alloc_test {
        ($invoke_context:ident, $memory_mapping:ident, $heap:ident) => {
            prepare_mockup!($invoke_context, program_id, bpf_loader::id());
            $invoke_context
                .set_syscall_context(SyscallContext {
                    allocator: BpfAllocator::new(solana_program_entrypoint::HEAP_LENGTH as u64),
                    accounts_metadata: Vec::new(),
                })
                .unwrap();
            let config = Config {
                aligned_memory_mapping: false,
                ..Config::default()
            };
            let mut $heap =
                AlignedMemory::<{ HOST_ALIGN }>::zero_filled(MAX_HEAP_FRAME_BYTES as usize);
            let regions = vec![MemoryRegion::new_writable(
                $heap.as_slice_mut(),
                ebpf::MM_HEAP_START,
            )];
            let mut $memory_mapping =
                MemoryMapping::new(regions, &config, SBPFVersion::V3).unwrap();
        };
    }

    #[test]
    fn test_syscall_sol_alloc_free() {
        // large alloc
        {
            setup_alloc_test!(invoke_context, memory_mapping, heap);
            let result = SyscallAllocFree::rust(
                &mut invoke_context,
                solana_program_entrypoint::HEAP_LENGTH as u64,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_ne!(result.unwrap(), 0);
            let result = SyscallAllocFree::rust(
                &mut invoke_context,
                solana_program_entrypoint::HEAP_LENGTH as u64,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
            let result = SyscallAllocFree::rust(
                &mut invoke_context,
                u64::MAX,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
        }

        // many small unaligned allocs
        {
            setup_alloc_test!(invoke_context, memory_mapping, heap);
            for _ in 0..100 {
                let result =
                    SyscallAllocFree::rust(&mut invoke_context, 1, 0, 0, 0, 0, &mut memory_mapping);
                assert_ne!(result.unwrap(), 0);
            }
            let result = SyscallAllocFree::rust(
                &mut invoke_context,
                solana_program_entrypoint::HEAP_LENGTH as u64,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
        }

        // many small aligned allocs
        {
            setup_alloc_test!(invoke_context, memory_mapping, heap);
            for _ in 0..12 {
                let result =
                    SyscallAllocFree::rust(&mut invoke_context, 1, 0, 0, 0, 0, &mut memory_mapping);
                assert_ne!(result.unwrap(), 0);
            }
            let result = SyscallAllocFree::rust(
                &mut invoke_context,
                solana_program_entrypoint::HEAP_LENGTH as u64,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
        }

        // aligned allocs

        fn aligned<T>() {
            setup_alloc_test!(invoke_context, memory_mapping, heap);
            let result = SyscallAllocFree::rust(
                &mut invoke_context,
                size_of::<T>() as u64,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            let address = result.unwrap();
            assert_ne!(address, 0);
            assert!(address_is_aligned::<T>(address));
        }
        aligned::<u8>();
        aligned::<u16>();
        aligned::<u32>();
        aligned::<u64>();
        aligned::<u128>();
    }

    #[test]
    fn test_syscall_sha256() {
        let config = Config::default();
        prepare_mockup!(invoke_context, program_id, bpf_loader_deprecated::id());

        let bytes1 = "Gaggablaghblagh!";
        let bytes2 = "flurbos";

        let mock_slice1 = MockSlice {
            vm_addr: 0x300000000,
            len: bytes1.len(),
        };
        let mock_slice2 = MockSlice {
            vm_addr: 0x400000000,
            len: bytes2.len(),
        };
        let bytes_to_hash = [mock_slice1, mock_slice2];
        let mut hash_result = [0; HASH_BYTES];
        let ro_len = bytes_to_hash.len() as u64;
        let ro_va = 0x100000000;
        let rw_va = 0x200000000;
        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(bytes_of_slice(&bytes_to_hash), ro_va),
                MemoryRegion::new_writable(bytes_of_slice_mut(&mut hash_result), rw_va),
                MemoryRegion::new_readonly(bytes1.as_bytes(), bytes_to_hash[0].vm_addr),
                MemoryRegion::new_readonly(bytes2.as_bytes(), bytes_to_hash[1].vm_addr),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        invoke_context.mock_set_remaining(
            (invoke_context.get_execution_cost().sha256_base_cost
                + invoke_context.get_execution_cost().mem_op_base_cost.max(
                    invoke_context
                        .get_execution_cost()
                        .sha256_byte_cost
                        .saturating_mul((bytes1.len() + bytes2.len()) as u64 / 2),
                ))
                * 4,
        );

        let result = SyscallHash::rust::<Sha256Hasher>(
            &mut invoke_context,
            ro_va,
            ro_len,
            rw_va,
            0,
            0,
            &mut memory_mapping,
        );
        result.unwrap();

        let hash_local = hashv(&[bytes1.as_ref(), bytes2.as_ref()]).to_bytes();
        assert_eq!(hash_result, hash_local);
        let result = SyscallHash::rust::<Sha256Hasher>(
            &mut invoke_context,
            ro_va - 1, // AccessViolation
            ro_len,
            rw_va,
            0,
            0,
            &mut memory_mapping,
        );
        assert_access_violation!(result, ro_va - 1, 32);
        let result = SyscallHash::rust::<Sha256Hasher>(
            &mut invoke_context,
            ro_va,
            ro_len + 1, // AccessViolation
            rw_va,
            0,
            0,
            &mut memory_mapping,
        );
        assert_access_violation!(result, ro_va, 48);
        let result = SyscallHash::rust::<Sha256Hasher>(
            &mut invoke_context,
            ro_va,
            ro_len,
            rw_va - 1, // AccessViolation
            0,
            0,
            &mut memory_mapping,
        );
        assert_access_violation!(result, rw_va - 1, HASH_BYTES as u64);
        let result = SyscallHash::rust::<Sha256Hasher>(
            &mut invoke_context,
            ro_va,
            ro_len,
            rw_va,
            0,
            0,
            &mut memory_mapping,
        );
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );
    }

    #[test]
    fn test_syscall_edwards_curve_point_validation() {
        use solana_curve25519::curve_syscall_traits::CURVE25519_EDWARDS;

        let config = Config::default();
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        let valid_bytes: [u8; 32] = [
            201, 179, 241, 122, 180, 185, 239, 50, 183, 52, 221, 0, 153, 195, 43, 18, 22, 38, 187,
            206, 179, 192, 210, 58, 53, 45, 150, 98, 89, 17, 158, 11,
        ];
        let valid_bytes_va = 0x100000000;

        let invalid_bytes: [u8; 32] = [
            120, 140, 152, 233, 41, 227, 203, 27, 87, 115, 25, 251, 219, 5, 84, 148, 117, 38, 84,
            60, 87, 144, 161, 146, 42, 34, 91, 155, 158, 189, 121, 79,
        ];
        let invalid_bytes_va = 0x200000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&valid_bytes, valid_bytes_va),
                MemoryRegion::new_readonly(&invalid_bytes, invalid_bytes_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        invoke_context.mock_set_remaining(
            (invoke_context
                .get_execution_cost()
                .curve25519_edwards_validate_point_cost)
                * 2,
        );

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            valid_bytes_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_eq!(0, result.unwrap());

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            invalid_bytes_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_eq!(1, result.unwrap());

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            valid_bytes_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );
    }

    #[test]
    fn test_syscall_ristretto_curve_point_validation() {
        use solana_curve25519::curve_syscall_traits::CURVE25519_RISTRETTO;

        let config = Config::default();
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        let valid_bytes: [u8; 32] = [
            226, 242, 174, 10, 106, 188, 78, 113, 168, 132, 169, 97, 197, 0, 81, 95, 88, 227, 11,
            106, 165, 130, 221, 141, 182, 166, 89, 69, 224, 141, 45, 118,
        ];
        let valid_bytes_va = 0x100000000;

        let invalid_bytes: [u8; 32] = [
            120, 140, 152, 233, 41, 227, 203, 27, 87, 115, 25, 251, 219, 5, 84, 148, 117, 38, 84,
            60, 87, 144, 161, 146, 42, 34, 91, 155, 158, 189, 121, 79,
        ];
        let invalid_bytes_va = 0x200000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&valid_bytes, valid_bytes_va),
                MemoryRegion::new_readonly(&invalid_bytes, invalid_bytes_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        invoke_context.mock_set_remaining(
            (invoke_context
                .get_execution_cost()
                .curve25519_ristretto_validate_point_cost)
                * 2,
        );

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            valid_bytes_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_eq!(0, result.unwrap());

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            invalid_bytes_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_eq!(1, result.unwrap());

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            valid_bytes_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );
    }

    #[test]
    fn test_syscall_edwards_curve_group_ops() {
        use solana_curve25519::curve_syscall_traits::{ADD, CURVE25519_EDWARDS, MUL, SUB};

        let config = Config::default();
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        let left_point: [u8; 32] = [
            33, 124, 71, 170, 117, 69, 151, 247, 59, 12, 95, 125, 133, 166, 64, 5, 2, 27, 90, 27,
            200, 167, 59, 164, 52, 54, 52, 200, 29, 13, 34, 213,
        ];
        let left_point_va = 0x100000000;
        let right_point: [u8; 32] = [
            70, 222, 137, 221, 253, 204, 71, 51, 78, 8, 124, 1, 67, 200, 102, 225, 122, 228, 111,
            183, 129, 14, 131, 210, 212, 95, 109, 246, 55, 10, 159, 91,
        ];
        let right_point_va = 0x200000000;
        let scalar: [u8; 32] = [
            254, 198, 23, 138, 67, 243, 184, 110, 236, 115, 236, 205, 205, 215, 79, 114, 45, 250,
            78, 137, 3, 107, 136, 237, 49, 126, 117, 223, 37, 191, 88, 6,
        ];
        let scalar_va = 0x300000000;
        let invalid_point: [u8; 32] = [
            120, 140, 152, 233, 41, 227, 203, 27, 87, 115, 25, 251, 219, 5, 84, 148, 117, 38, 84,
            60, 87, 144, 161, 146, 42, 34, 91, 155, 158, 189, 121, 79,
        ];
        let invalid_point_va = 0x400000000;
        let mut result_point: [u8; 32] = [0; 32];
        let result_point_va = 0x500000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(bytes_of_slice(&left_point), left_point_va),
                MemoryRegion::new_readonly(bytes_of_slice(&right_point), right_point_va),
                MemoryRegion::new_readonly(bytes_of_slice(&scalar), scalar_va),
                MemoryRegion::new_readonly(bytes_of_slice(&invalid_point), invalid_point_va),
                MemoryRegion::new_writable(bytes_of_slice_mut(&mut result_point), result_point_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        invoke_context.mock_set_remaining(
            (invoke_context
                .get_execution_cost()
                .curve25519_edwards_add_cost
                + invoke_context
                    .get_execution_cost()
                    .curve25519_edwards_subtract_cost
                + invoke_context
                    .get_execution_cost()
                    .curve25519_edwards_multiply_cost)
                * 2,
        );

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            ADD,
            left_point_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        let expected_sum = [
            7, 251, 187, 86, 186, 232, 57, 242, 193, 236, 49, 200, 90, 29, 254, 82, 46, 80, 83, 70,
            244, 153, 23, 156, 2, 138, 207, 51, 165, 38, 200, 85,
        ];
        assert_eq!(expected_sum, result_point);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            ADD,
            invalid_point_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );
        assert_eq!(1, result.unwrap());

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            SUB,
            left_point_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        let expected_difference = [
            60, 87, 90, 68, 232, 25, 7, 172, 247, 120, 158, 104, 52, 127, 94, 244, 5, 79, 253, 15,
            48, 69, 82, 134, 155, 70, 188, 81, 108, 95, 212, 9,
        ];
        assert_eq!(expected_difference, result_point);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            SUB,
            invalid_point_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );
        assert_eq!(1, result.unwrap());

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            MUL,
            scalar_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );

        result.unwrap();
        let expected_product = [
            64, 150, 40, 55, 80, 49, 217, 209, 105, 229, 181, 65, 241, 68, 2, 106, 220, 234, 211,
            71, 159, 76, 156, 114, 242, 68, 147, 31, 243, 211, 191, 124,
        ];
        assert_eq!(expected_product, result_point);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            MUL,
            scalar_va,
            invalid_point_va,
            result_point_va,
            &mut memory_mapping,
        );
        assert_eq!(1, result.unwrap());

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            MUL,
            scalar_va,
            invalid_point_va,
            result_point_va,
            &mut memory_mapping,
        );
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );
    }

    #[test]
    fn test_syscall_ristretto_curve_group_ops() {
        use solana_curve25519::curve_syscall_traits::{ADD, CURVE25519_RISTRETTO, MUL, SUB};

        let config = Config::default();
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        let left_point: [u8; 32] = [
            208, 165, 125, 204, 2, 100, 218, 17, 170, 194, 23, 9, 102, 156, 134, 136, 217, 190, 98,
            34, 183, 194, 228, 153, 92, 11, 108, 103, 28, 57, 88, 15,
        ];
        let left_point_va = 0x100000000;
        let right_point: [u8; 32] = [
            208, 241, 72, 163, 73, 53, 32, 174, 54, 194, 71, 8, 70, 181, 244, 199, 93, 147, 99,
            231, 162, 127, 25, 40, 39, 19, 140, 132, 112, 212, 145, 108,
        ];
        let right_point_va = 0x200000000;
        let scalar: [u8; 32] = [
            254, 198, 23, 138, 67, 243, 184, 110, 236, 115, 236, 205, 205, 215, 79, 114, 45, 250,
            78, 137, 3, 107, 136, 237, 49, 126, 117, 223, 37, 191, 88, 6,
        ];
        let scalar_va = 0x300000000;
        let invalid_point: [u8; 32] = [
            120, 140, 152, 233, 41, 227, 203, 27, 87, 115, 25, 251, 219, 5, 84, 148, 117, 38, 84,
            60, 87, 144, 161, 146, 42, 34, 91, 155, 158, 189, 121, 79,
        ];
        let invalid_point_va = 0x400000000;
        let mut result_point: [u8; 32] = [0; 32];
        let result_point_va = 0x500000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(bytes_of_slice(&left_point), left_point_va),
                MemoryRegion::new_readonly(bytes_of_slice(&right_point), right_point_va),
                MemoryRegion::new_readonly(bytes_of_slice(&scalar), scalar_va),
                MemoryRegion::new_readonly(bytes_of_slice(&invalid_point), invalid_point_va),
                MemoryRegion::new_writable(bytes_of_slice_mut(&mut result_point), result_point_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        invoke_context.mock_set_remaining(
            (invoke_context
                .get_execution_cost()
                .curve25519_ristretto_add_cost
                + invoke_context
                    .get_execution_cost()
                    .curve25519_ristretto_subtract_cost
                + invoke_context
                    .get_execution_cost()
                    .curve25519_ristretto_multiply_cost)
                * 2,
        );

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            ADD,
            left_point_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        let expected_sum = [
            78, 173, 9, 241, 180, 224, 31, 107, 176, 210, 144, 240, 118, 73, 70, 191, 128, 119,
            141, 113, 125, 215, 161, 71, 49, 176, 87, 38, 180, 177, 39, 78,
        ];
        assert_eq!(expected_sum, result_point);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            ADD,
            invalid_point_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );
        assert_eq!(1, result.unwrap());

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            SUB,
            left_point_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        let expected_difference = [
            150, 72, 222, 61, 148, 79, 96, 130, 151, 176, 29, 217, 231, 211, 0, 215, 76, 86, 212,
            146, 110, 128, 24, 151, 187, 144, 108, 233, 221, 208, 157, 52,
        ];
        assert_eq!(expected_difference, result_point);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            SUB,
            invalid_point_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(1, result.unwrap());

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            MUL,
            scalar_va,
            right_point_va,
            result_point_va,
            &mut memory_mapping,
        );

        result.unwrap();
        let expected_product = [
            4, 16, 46, 2, 53, 151, 201, 133, 117, 149, 232, 164, 119, 109, 136, 20, 153, 24, 124,
            21, 101, 124, 80, 19, 119, 100, 77, 108, 65, 187, 228, 5,
        ];
        assert_eq!(expected_product, result_point);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            MUL,
            scalar_va,
            invalid_point_va,
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(1, result.unwrap());

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            MUL,
            scalar_va,
            invalid_point_va,
            result_point_va,
            &mut memory_mapping,
        );
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );
    }

    #[test]
    fn test_syscall_multiscalar_multiplication() {
        use solana_curve25519::curve_syscall_traits::{CURVE25519_EDWARDS, CURVE25519_RISTRETTO};

        let config = Config::default();
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        let scalar_a: [u8; 32] = [
            254, 198, 23, 138, 67, 243, 184, 110, 236, 115, 236, 205, 205, 215, 79, 114, 45, 250,
            78, 137, 3, 107, 136, 237, 49, 126, 117, 223, 37, 191, 88, 6,
        ];
        let scalar_b: [u8; 32] = [
            254, 198, 23, 138, 67, 243, 184, 110, 236, 115, 236, 205, 205, 215, 79, 114, 45, 250,
            78, 137, 3, 107, 136, 237, 49, 126, 117, 223, 37, 191, 88, 6,
        ];

        let scalars = [scalar_a, scalar_b];
        let scalars_va = 0x100000000;

        let edwards_point_x: [u8; 32] = [
            252, 31, 230, 46, 173, 95, 144, 148, 158, 157, 63, 10, 8, 68, 58, 176, 142, 192, 168,
            53, 61, 105, 194, 166, 43, 56, 246, 236, 28, 146, 114, 133,
        ];
        let edwards_point_y: [u8; 32] = [
            10, 111, 8, 236, 97, 189, 124, 69, 89, 176, 222, 39, 199, 253, 111, 11, 248, 186, 128,
            90, 120, 128, 248, 210, 232, 183, 93, 104, 111, 150, 7, 241,
        ];
        let edwards_points = [edwards_point_x, edwards_point_y];
        let edwards_points_va = 0x200000000;

        let ristretto_point_x: [u8; 32] = [
            130, 35, 97, 25, 18, 199, 33, 239, 85, 143, 119, 111, 49, 51, 224, 40, 167, 185, 240,
            179, 25, 194, 213, 41, 14, 155, 104, 18, 181, 197, 15, 112,
        ];
        let ristretto_point_y: [u8; 32] = [
            152, 156, 155, 197, 152, 232, 92, 206, 219, 159, 193, 134, 121, 128, 139, 36, 56, 191,
            51, 143, 72, 204, 87, 76, 110, 124, 101, 96, 238, 158, 42, 108,
        ];
        let ristretto_points = [ristretto_point_x, ristretto_point_y];
        let ristretto_points_va = 0x300000000;

        let mut result_point: [u8; 32] = [0; 32];
        let result_point_va = 0x400000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(bytes_of_slice(&scalars), scalars_va),
                MemoryRegion::new_readonly(bytes_of_slice(&edwards_points), edwards_points_va),
                MemoryRegion::new_readonly(bytes_of_slice(&ristretto_points), ristretto_points_va),
                MemoryRegion::new_writable(bytes_of_slice_mut(&mut result_point), result_point_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        invoke_context.mock_set_remaining(
            invoke_context
                .get_execution_cost()
                .curve25519_edwards_msm_base_cost
                + invoke_context
                    .get_execution_cost()
                    .curve25519_edwards_msm_incremental_cost
                + invoke_context
                    .get_execution_cost()
                    .curve25519_ristretto_msm_base_cost
                + invoke_context
                    .get_execution_cost()
                    .curve25519_ristretto_msm_incremental_cost,
        );

        let result = SyscallCurveMultiscalarMultiplication::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            scalars_va,
            edwards_points_va,
            2,
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        let expected_product = [
            30, 174, 168, 34, 160, 70, 63, 166, 236, 18, 74, 144, 185, 222, 208, 243, 5, 54, 223,
            172, 185, 75, 244, 26, 70, 18, 248, 46, 207, 184, 235, 60,
        ];
        assert_eq!(expected_product, result_point);

        let result = SyscallCurveMultiscalarMultiplication::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            scalars_va,
            ristretto_points_va,
            2,
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        let expected_product = [
            78, 120, 86, 111, 152, 64, 146, 84, 14, 236, 77, 147, 237, 190, 251, 241, 136, 167, 21,
            94, 84, 118, 92, 140, 120, 81, 30, 246, 173, 140, 195, 86,
        ];
        assert_eq!(expected_product, result_point);
    }

    #[test]
    fn test_syscall_multiscalar_multiplication_maximum_length_exceeded() {
        use solana_curve25519::curve_syscall_traits::{CURVE25519_EDWARDS, CURVE25519_RISTRETTO};

        let config = Config::default();
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        let scalar: [u8; 32] = [
            254, 198, 23, 138, 67, 243, 184, 110, 236, 115, 236, 205, 205, 215, 79, 114, 45, 250,
            78, 137, 3, 107, 136, 237, 49, 126, 117, 223, 37, 191, 88, 6,
        ];
        let scalars = [scalar; 513];
        let scalars_va = 0x100000000;

        let edwards_point: [u8; 32] = [
            252, 31, 230, 46, 173, 95, 144, 148, 158, 157, 63, 10, 8, 68, 58, 176, 142, 192, 168,
            53, 61, 105, 194, 166, 43, 56, 246, 236, 28, 146, 114, 133,
        ];
        let edwards_points = [edwards_point; 513];
        let edwards_points_va = 0x200000000;

        let ristretto_point: [u8; 32] = [
            130, 35, 97, 25, 18, 199, 33, 239, 85, 143, 119, 111, 49, 51, 224, 40, 167, 185, 240,
            179, 25, 194, 213, 41, 14, 155, 104, 18, 181, 197, 15, 112,
        ];
        let ristretto_points = [ristretto_point; 513];
        let ristretto_points_va = 0x300000000;

        let mut result_point: [u8; 32] = [0; 32];
        let result_point_va = 0x400000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(bytes_of_slice(&scalars), scalars_va),
                MemoryRegion::new_readonly(bytes_of_slice(&edwards_points), edwards_points_va),
                MemoryRegion::new_readonly(bytes_of_slice(&ristretto_points), ristretto_points_va),
                MemoryRegion::new_writable(bytes_of_slice_mut(&mut result_point), result_point_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        // test Edwards
        invoke_context.mock_set_remaining(500_000);
        let result = SyscallCurveMultiscalarMultiplication::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            scalars_va,
            edwards_points_va,
            512, // below maximum vector length
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        let expected_product = [
            20, 146, 226, 37, 22, 61, 86, 249, 208, 40, 38, 11, 126, 101, 10, 82, 81, 77, 88, 209,
            15, 76, 82, 251, 180, 133, 84, 243, 162, 0, 11, 145,
        ];
        assert_eq!(expected_product, result_point);

        invoke_context.mock_set_remaining(500_000);
        let result = SyscallCurveMultiscalarMultiplication::rust(
            &mut invoke_context,
            CURVE25519_EDWARDS,
            scalars_va,
            edwards_points_va,
            513, // above maximum vector length
            result_point_va,
            &mut memory_mapping,
        )
        .unwrap_err()
        .downcast::<SyscallError>()
        .unwrap();

        assert_eq!(*result, SyscallError::InvalidLength);

        // test Ristretto
        invoke_context.mock_set_remaining(500_000);
        let result = SyscallCurveMultiscalarMultiplication::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            scalars_va,
            ristretto_points_va,
            512, // below maximum vector length
            result_point_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        let expected_product = [
            146, 224, 127, 193, 252, 64, 196, 181, 246, 104, 27, 116, 183, 52, 200, 239, 2, 108,
            21, 27, 97, 44, 95, 65, 26, 218, 223, 39, 197, 132, 51, 49,
        ];
        assert_eq!(expected_product, result_point);

        invoke_context.mock_set_remaining(500_000);
        let result = SyscallCurveMultiscalarMultiplication::rust(
            &mut invoke_context,
            CURVE25519_RISTRETTO,
            scalars_va,
            ristretto_points_va,
            513, // above maximum vector length
            result_point_va,
            &mut memory_mapping,
        )
        .unwrap_err()
        .downcast::<SyscallError>()
        .unwrap();

        assert_eq!(*result, SyscallError::InvalidLength);
    }

    fn create_filled_type<T: Default>(zero_init: bool) -> T {
        let mut val = T::default();
        let p = &mut val as *mut _ as *mut u8;
        for i in 0..(size_of::<T>() as isize) {
            unsafe {
                *p.offset(i) = if zero_init { 0 } else { i as u8 };
            }
        }
        val
    }

    fn are_bytes_equal<T>(first: &T, second: &T) -> bool {
        let p_first = first as *const _ as *const u8;
        let p_second = second as *const _ as *const u8;

        for i in 0..(size_of::<T>() as isize) {
            unsafe {
                if *p_first.offset(i) != *p_second.offset(i) {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    #[allow(deprecated)]
    fn test_syscall_get_sysvar() {
        let config = Config::default();

        let mut src_clock = create_filled_type::<Clock>(false);
        src_clock.slot = 1;
        src_clock.epoch_start_timestamp = 2;
        src_clock.epoch = 3;
        src_clock.leader_schedule_epoch = 4;
        src_clock.unix_timestamp = 5;

        let mut src_epochschedule = create_filled_type::<EpochSchedule>(false);
        src_epochschedule.slots_per_epoch = 1;
        src_epochschedule.leader_schedule_slot_offset = 2;
        src_epochschedule.warmup = false;
        src_epochschedule.first_normal_epoch = 3;
        src_epochschedule.first_normal_slot = 4;

        let mut src_fees = create_filled_type::<Fees>(false);
        src_fees.fee_calculator = FeeCalculator {
            lamports_per_signature: 1,
        };

        let mut src_rent = create_filled_type::<Rent>(false);
        src_rent.lamports_per_byte_year = 1;
        src_rent.exemption_threshold = 2.0;
        src_rent.burn_percent = 3;

        let mut src_rewards = create_filled_type::<EpochRewards>(false);
        src_rewards.distribution_starting_block_height = 42;
        src_rewards.num_partitions = 2;
        src_rewards.parent_blockhash = Hash::new_from_array([3; 32]);
        src_rewards.total_points = 4;
        src_rewards.total_rewards = 100;
        src_rewards.distributed_rewards = 10;
        src_rewards.active = true;

        let mut src_restart = create_filled_type::<LastRestartSlot>(false);
        src_restart.last_restart_slot = 1;

        let transaction_accounts = vec![
            (
                sysvar::clock::id(),
                create_account_shared_data_for_test(&src_clock),
            ),
            (
                sysvar::epoch_schedule::id(),
                create_account_shared_data_for_test(&src_epochschedule),
            ),
            (
                sysvar::fees::id(),
                create_account_shared_data_for_test(&src_fees),
            ),
            (
                sysvar::rent::id(),
                create_account_shared_data_for_test(&src_rent),
            ),
            (
                sysvar::epoch_rewards::id(),
                create_account_shared_data_for_test(&src_rewards),
            ),
            (
                sysvar::last_restart_slot::id(),
                create_account_shared_data_for_test(&src_restart),
            ),
        ];
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);

        // Test clock sysvar
        {
            let mut got_clock_obj = Clock::default();
            let got_clock_obj_va = 0x100000000;

            let mut got_clock_buf = vec![0; Clock::size_of()];
            let got_clock_buf_va = 0x200000000;
            let clock_id_va = 0x300000000;
            let clock_id = Clock::id().to_bytes();

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    MemoryRegion::new_writable(bytes_of_mut(&mut got_clock_obj), got_clock_obj_va),
                    MemoryRegion::new_writable(&mut got_clock_buf, got_clock_buf_va),
                    MemoryRegion::new_readonly(&clock_id, clock_id_va),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetClockSysvar::rust(
                &mut invoke_context,
                got_clock_obj_va,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
            assert_eq!(got_clock_obj, src_clock);

            let mut clean_clock = create_filled_type::<Clock>(true);
            clean_clock.slot = src_clock.slot;
            clean_clock.epoch_start_timestamp = src_clock.epoch_start_timestamp;
            clean_clock.epoch = src_clock.epoch;
            clean_clock.leader_schedule_epoch = src_clock.leader_schedule_epoch;
            clean_clock.unix_timestamp = src_clock.unix_timestamp;
            assert!(are_bytes_equal(&got_clock_obj, &clean_clock));

            let result = SyscallGetSysvar::rust(
                &mut invoke_context,
                clock_id_va,
                got_clock_buf_va,
                0,
                Clock::size_of() as u64,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);

            let clock_from_buf = bincode::deserialize::<Clock>(&got_clock_buf).unwrap();

            assert_eq!(clock_from_buf, src_clock);
            assert!(are_bytes_equal(&clock_from_buf, &clean_clock));
        }

        // Test epoch_schedule sysvar
        {
            let mut got_epochschedule_obj = EpochSchedule::default();
            let got_epochschedule_obj_va = 0x100000000;

            let mut got_epochschedule_buf = vec![0; EpochSchedule::size_of()];
            let got_epochschedule_buf_va = 0x200000000;
            let epochschedule_id_va = 0x300000000;
            let epochschedule_id = EpochSchedule::id().to_bytes();

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    MemoryRegion::new_writable(
                        bytes_of_mut(&mut got_epochschedule_obj),
                        got_epochschedule_obj_va,
                    ),
                    MemoryRegion::new_writable(
                        &mut got_epochschedule_buf,
                        got_epochschedule_buf_va,
                    ),
                    MemoryRegion::new_readonly(&epochschedule_id, epochschedule_id_va),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetEpochScheduleSysvar::rust(
                &mut invoke_context,
                got_epochschedule_obj_va,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
            assert_eq!(got_epochschedule_obj, src_epochschedule);

            let mut clean_epochschedule = create_filled_type::<EpochSchedule>(true);
            clean_epochschedule.slots_per_epoch = src_epochschedule.slots_per_epoch;
            clean_epochschedule.leader_schedule_slot_offset =
                src_epochschedule.leader_schedule_slot_offset;
            clean_epochschedule.warmup = src_epochschedule.warmup;
            clean_epochschedule.first_normal_epoch = src_epochschedule.first_normal_epoch;
            clean_epochschedule.first_normal_slot = src_epochschedule.first_normal_slot;
            assert!(are_bytes_equal(
                &got_epochschedule_obj,
                &clean_epochschedule
            ));

            let result = SyscallGetSysvar::rust(
                &mut invoke_context,
                epochschedule_id_va,
                got_epochschedule_buf_va,
                0,
                EpochSchedule::size_of() as u64,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);

            let epochschedule_from_buf =
                bincode::deserialize::<EpochSchedule>(&got_epochschedule_buf).unwrap();

            assert_eq!(epochschedule_from_buf, src_epochschedule);

            // clone is to zero the alignment padding
            assert!(are_bytes_equal(
                &epochschedule_from_buf.clone(),
                &clean_epochschedule
            ));
        }

        // Test fees sysvar
        {
            let mut got_fees = Fees::default();
            let got_fees_va = 0x100000000;

            let mut memory_mapping = MemoryMapping::new(
                vec![MemoryRegion::new_writable(
                    bytes_of_mut(&mut got_fees),
                    got_fees_va,
                )],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetFeesSysvar::rust(
                &mut invoke_context,
                got_fees_va,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
            assert_eq!(got_fees, src_fees);

            let mut clean_fees = create_filled_type::<Fees>(true);
            clean_fees.fee_calculator = src_fees.fee_calculator;
            assert!(are_bytes_equal(&got_fees, &clean_fees));

            // fees sysvar is not accessible via sol_get_sysvar so nothing further to test
        }

        // Test rent sysvar
        {
            let mut got_rent_obj = create_filled_type::<Rent>(true);
            let got_rent_obj_va = 0x100000000;

            let mut got_rent_buf = vec![0; Rent::size_of()];
            let got_rent_buf_va = 0x200000000;
            let rent_id_va = 0x300000000;
            let rent_id = Rent::id().to_bytes();

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    MemoryRegion::new_writable(bytes_of_mut(&mut got_rent_obj), got_rent_obj_va),
                    MemoryRegion::new_writable(&mut got_rent_buf, got_rent_buf_va),
                    MemoryRegion::new_readonly(&rent_id, rent_id_va),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetRentSysvar::rust(
                &mut invoke_context,
                got_rent_obj_va,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
            assert_eq!(got_rent_obj, src_rent);

            let mut clean_rent = create_filled_type::<Rent>(true);
            clean_rent.lamports_per_byte_year = src_rent.lamports_per_byte_year;
            clean_rent.exemption_threshold = src_rent.exemption_threshold;
            clean_rent.burn_percent = src_rent.burn_percent;
            assert!(are_bytes_equal(&got_rent_obj, &clean_rent));

            let result = SyscallGetSysvar::rust(
                &mut invoke_context,
                rent_id_va,
                got_rent_buf_va,
                0,
                Rent::size_of() as u64,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);

            let rent_from_buf = bincode::deserialize::<Rent>(&got_rent_buf).unwrap();

            assert_eq!(rent_from_buf, src_rent);

            // clone is to zero the alignment padding
            assert!(are_bytes_equal(&rent_from_buf.clone(), &clean_rent));
        }

        // Test epoch rewards sysvar
        {
            let mut got_rewards_obj = create_filled_type::<EpochRewards>(true);
            let got_rewards_obj_va = 0x100000000;

            let mut got_rewards_buf = vec![0; EpochRewards::size_of()];
            let got_rewards_buf_va = 0x200000000;
            let rewards_id_va = 0x300000000;
            let rewards_id = EpochRewards::id().to_bytes();

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    MemoryRegion::new_writable(
                        bytes_of_mut(&mut got_rewards_obj),
                        got_rewards_obj_va,
                    ),
                    MemoryRegion::new_writable(&mut got_rewards_buf, got_rewards_buf_va),
                    MemoryRegion::new_readonly(&rewards_id, rewards_id_va),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetEpochRewardsSysvar::rust(
                &mut invoke_context,
                got_rewards_obj_va,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
            assert_eq!(got_rewards_obj, src_rewards);

            let mut clean_rewards = create_filled_type::<EpochRewards>(true);
            clean_rewards.distribution_starting_block_height =
                src_rewards.distribution_starting_block_height;
            clean_rewards.num_partitions = src_rewards.num_partitions;
            clean_rewards.parent_blockhash = src_rewards.parent_blockhash;
            clean_rewards.total_points = src_rewards.total_points;
            clean_rewards.total_rewards = src_rewards.total_rewards;
            clean_rewards.distributed_rewards = src_rewards.distributed_rewards;
            clean_rewards.active = src_rewards.active;
            assert!(are_bytes_equal(&got_rewards_obj, &clean_rewards));

            let result = SyscallGetSysvar::rust(
                &mut invoke_context,
                rewards_id_va,
                got_rewards_buf_va,
                0,
                EpochRewards::size_of() as u64,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);

            let rewards_from_buf = bincode::deserialize::<EpochRewards>(&got_rewards_buf).unwrap();

            assert_eq!(rewards_from_buf, src_rewards);

            // clone is to zero the alignment padding
            assert!(are_bytes_equal(&rewards_from_buf.clone(), &clean_rewards));
        }

        // Test last restart slot sysvar
        {
            let mut got_restart_obj = LastRestartSlot::default();
            let got_restart_obj_va = 0x100000000;

            let mut got_restart_buf = vec![0; LastRestartSlot::size_of()];
            let got_restart_buf_va = 0x200000000;
            let restart_id_va = 0x300000000;
            let restart_id = LastRestartSlot::id().to_bytes();

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    MemoryRegion::new_writable(
                        bytes_of_mut(&mut got_restart_obj),
                        got_restart_obj_va,
                    ),
                    MemoryRegion::new_writable(&mut got_restart_buf, got_restart_buf_va),
                    MemoryRegion::new_readonly(&restart_id, restart_id_va),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetLastRestartSlotSysvar::rust(
                &mut invoke_context,
                got_restart_obj_va,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);
            assert_eq!(got_restart_obj, src_restart);

            let mut clean_restart = create_filled_type::<LastRestartSlot>(true);
            clean_restart.last_restart_slot = src_restart.last_restart_slot;
            assert!(are_bytes_equal(&got_restart_obj, &clean_restart));

            let result = SyscallGetSysvar::rust(
                &mut invoke_context,
                restart_id_va,
                got_restart_buf_va,
                0,
                LastRestartSlot::size_of() as u64,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);

            let restart_from_buf =
                bincode::deserialize::<LastRestartSlot>(&got_restart_buf).unwrap();

            assert_eq!(restart_from_buf, src_restart);
            assert!(are_bytes_equal(&restart_from_buf, &clean_restart));
        }
    }

    #[test_case(false; "partial")]
    #[test_case(true; "full")]
    fn test_syscall_get_stake_history(filled: bool) {
        let config = Config::default();

        let mut src_history = StakeHistory::default();

        let epochs = if filled {
            stake_history::MAX_ENTRIES + 1
        } else {
            stake_history::MAX_ENTRIES / 2
        } as u64;

        for epoch in 1..epochs {
            src_history.add(
                epoch,
                StakeHistoryEntry {
                    effective: epoch * 2,
                    activating: epoch * 3,
                    deactivating: epoch * 5,
                },
            );
        }

        let src_history = src_history;

        let mut src_history_buf = vec![0; StakeHistory::size_of()];
        bincode::serialize_into(&mut src_history_buf, &src_history).unwrap();

        let transaction_accounts = vec![(
            sysvar::stake_history::id(),
            create_account_shared_data_for_test(&src_history),
        )];
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);

        {
            let mut got_history_buf = vec![0; StakeHistory::size_of()];
            let got_history_buf_va = 0x100000000;
            let history_id_va = 0x200000000;
            let history_id = StakeHistory::id().to_bytes();

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    MemoryRegion::new_writable(&mut got_history_buf, got_history_buf_va),
                    MemoryRegion::new_readonly(&history_id, history_id_va),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetSysvar::rust(
                &mut invoke_context,
                history_id_va,
                got_history_buf_va,
                0,
                StakeHistory::size_of() as u64,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);

            let history_from_buf = bincode::deserialize::<StakeHistory>(&got_history_buf).unwrap();
            assert_eq!(history_from_buf, src_history);
        }
    }

    #[test_case(false; "partial")]
    #[test_case(true; "full")]
    fn test_syscall_get_slot_hashes(filled: bool) {
        let config = Config::default();

        let mut src_hashes = SlotHashes::default();

        let slots = if filled {
            slot_hashes::MAX_ENTRIES + 1
        } else {
            slot_hashes::MAX_ENTRIES / 2
        } as u64;

        for slot in 1..slots {
            src_hashes.add(slot, hashv(&[&slot.to_le_bytes()]));
        }

        let src_hashes = src_hashes;

        let mut src_hashes_buf = vec![0; SlotHashes::size_of()];
        bincode::serialize_into(&mut src_hashes_buf, &src_hashes).unwrap();

        let transaction_accounts = vec![(
            sysvar::slot_hashes::id(),
            create_account_shared_data_for_test(&src_hashes),
        )];
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);

        {
            let mut got_hashes_buf = vec![0; SlotHashes::size_of()];
            let got_hashes_buf_va = 0x100000000;
            let hashes_id_va = 0x200000000;
            let hashes_id = SlotHashes::id().to_bytes();

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    MemoryRegion::new_writable(&mut got_hashes_buf, got_hashes_buf_va),
                    MemoryRegion::new_readonly(&hashes_id, hashes_id_va),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetSysvar::rust(
                &mut invoke_context,
                hashes_id_va,
                got_hashes_buf_va,
                0,
                SlotHashes::size_of() as u64,
                0,
                &mut memory_mapping,
            );
            assert_eq!(result.unwrap(), 0);

            let hashes_from_buf = bincode::deserialize::<SlotHashes>(&got_hashes_buf).unwrap();
            assert_eq!(hashes_from_buf, src_hashes);
        }
    }

    #[test]
    fn test_syscall_get_sysvar_errors() {
        let config = Config::default();

        let mut src_clock = create_filled_type::<Clock>(false);
        src_clock.slot = 1;
        src_clock.epoch_start_timestamp = 2;
        src_clock.epoch = 3;
        src_clock.leader_schedule_epoch = 4;
        src_clock.unix_timestamp = 5;

        let clock_id_va = 0x100000000;
        let clock_id = Clock::id().to_bytes();

        let mut got_clock_buf_rw = vec![0; Clock::size_of()];
        let got_clock_buf_rw_va = 0x200000000;

        let got_clock_buf_ro = vec![0; Clock::size_of()];
        let got_clock_buf_ro_va = 0x300000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&clock_id, clock_id_va),
                MemoryRegion::new_writable(&mut got_clock_buf_rw, got_clock_buf_rw_va),
                MemoryRegion::new_readonly(&got_clock_buf_ro, got_clock_buf_ro_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let access_violation_err =
            std::mem::discriminant(&EbpfError::AccessViolation(AccessType::Load, 0, 0, ""));

        let got_clock_empty = vec![0; Clock::size_of()];

        {
            // start without the clock sysvar because we expect to hit specific errors before loading it
            with_mock_invoke_context!(invoke_context, transaction_context, vec![]);

            // Abort: "Not all bytes in VM memory range `[sysvar_id, sysvar_id + 32)` are readable."
            let e = SyscallGetSysvar::rust(
                &mut invoke_context,
                clock_id_va + 1,
                got_clock_buf_rw_va,
                0,
                Clock::size_of() as u64,
                0,
                &mut memory_mapping,
            )
            .unwrap_err();

            assert_eq!(
                std::mem::discriminant(e.downcast_ref::<EbpfError>().unwrap()),
                access_violation_err,
            );
            assert_eq!(got_clock_buf_rw, got_clock_empty);

            // Abort: "Not all bytes in VM memory range `[var_addr, var_addr + length)` are writable."
            let e = SyscallGetSysvar::rust(
                &mut invoke_context,
                clock_id_va,
                got_clock_buf_rw_va + 1,
                0,
                Clock::size_of() as u64,
                0,
                &mut memory_mapping,
            )
            .unwrap_err();

            assert_eq!(
                std::mem::discriminant(e.downcast_ref::<EbpfError>().unwrap()),
                access_violation_err,
            );
            assert_eq!(got_clock_buf_rw, got_clock_empty);

            let e = SyscallGetSysvar::rust(
                &mut invoke_context,
                clock_id_va,
                got_clock_buf_ro_va,
                0,
                Clock::size_of() as u64,
                0,
                &mut memory_mapping,
            )
            .unwrap_err();

            assert_eq!(
                std::mem::discriminant(e.downcast_ref::<EbpfError>().unwrap()),
                access_violation_err,
            );
            assert_eq!(got_clock_buf_rw, got_clock_empty);

            // Abort: "`offset + length` is not in `[0, 2^64)`."
            let e = SyscallGetSysvar::rust(
                &mut invoke_context,
                clock_id_va,
                got_clock_buf_rw_va,
                u64::MAX - Clock::size_of() as u64 / 2,
                Clock::size_of() as u64,
                0,
                &mut memory_mapping,
            )
            .unwrap_err();

            assert_eq!(
                *e.downcast_ref::<InstructionError>().unwrap(),
                InstructionError::ArithmeticOverflow,
            );
            assert_eq!(got_clock_buf_rw, got_clock_empty);

            // "`var_addr + length` is not in `[0, 2^64)`" is theoretically impossible to trigger
            // because if the sum extended outside u64::MAX then it would not be writable and translate would fail

            // "`2` if the sysvar data is not present in the Sysvar Cache."
            let result = SyscallGetSysvar::rust(
                &mut invoke_context,
                clock_id_va,
                got_clock_buf_rw_va,
                0,
                Clock::size_of() as u64,
                0,
                &mut memory_mapping,
            )
            .unwrap();

            assert_eq!(result, 2);
            assert_eq!(got_clock_buf_rw, got_clock_empty);
        }

        {
            let transaction_accounts = vec![(
                sysvar::clock::id(),
                create_account_shared_data_for_test(&src_clock),
            )];
            with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);

            // "`1` if `offset + length` is greater than the length of the sysvar data."
            let result = SyscallGetSysvar::rust(
                &mut invoke_context,
                clock_id_va,
                got_clock_buf_rw_va,
                1,
                Clock::size_of() as u64,
                0,
                &mut memory_mapping,
            )
            .unwrap();

            assert_eq!(result, 1);
            assert_eq!(got_clock_buf_rw, got_clock_empty);

            // and now lets succeed
            SyscallGetSysvar::rust(
                &mut invoke_context,
                clock_id_va,
                got_clock_buf_rw_va,
                0,
                Clock::size_of() as u64,
                0,
                &mut memory_mapping,
            )
            .unwrap();

            let clock_from_buf = bincode::deserialize::<Clock>(&got_clock_buf_rw).unwrap();

            assert_eq!(clock_from_buf, src_clock);
        }
    }

    type BuiltinFunctionRustInterface<'a> = fn(
        &mut InvokeContext<'a, 'a>,
        u64,
        u64,
        u64,
        u64,
        u64,
        &mut MemoryMapping,
    ) -> Result<u64, Box<dyn std::error::Error>>;

    fn call_program_address_common<'a, 'b: 'a>(
        invoke_context: &'a mut InvokeContext<'b, 'b>,
        seeds: &[&[u8]],
        program_id: &Pubkey,
        overlap_outputs: bool,
        syscall: BuiltinFunctionRustInterface<'b>,
    ) -> Result<(Pubkey, u8), Error> {
        const SEEDS_VA: u64 = 0x100000000;
        const PROGRAM_ID_VA: u64 = 0x200000000;
        const ADDRESS_VA: u64 = 0x300000000;
        const BUMP_SEED_VA: u64 = 0x400000000;
        const SEED_VA: u64 = 0x500000000;

        let config = Config::default();
        let mut address = Pubkey::default();
        let mut bump_seed = 0;
        let mut regions = vec![
            MemoryRegion::new_readonly(bytes_of(program_id), PROGRAM_ID_VA),
            MemoryRegion::new_writable(bytes_of_mut(&mut address), ADDRESS_VA),
            MemoryRegion::new_writable(bytes_of_mut(&mut bump_seed), BUMP_SEED_VA),
        ];

        let mut mock_slices = Vec::with_capacity(seeds.len());
        for (i, seed) in seeds.iter().enumerate() {
            let vm_addr = SEED_VA.saturating_add((i as u64).saturating_mul(0x100000000));
            let mock_slice = MockSlice {
                vm_addr,
                len: seed.len(),
            };
            mock_slices.push(mock_slice);
            regions.push(MemoryRegion::new_readonly(bytes_of_slice(seed), vm_addr));
        }
        regions.push(MemoryRegion::new_readonly(
            bytes_of_slice(&mock_slices),
            SEEDS_VA,
        ));
        let mut memory_mapping = MemoryMapping::new(regions, &config, SBPFVersion::V3).unwrap();

        let result = syscall(
            invoke_context,
            SEEDS_VA,
            seeds.len() as u64,
            PROGRAM_ID_VA,
            ADDRESS_VA,
            if overlap_outputs {
                ADDRESS_VA
            } else {
                BUMP_SEED_VA
            },
            &mut memory_mapping,
        );
        result.map(|_| (address, bump_seed))
    }

    fn create_program_address<'a>(
        invoke_context: &mut InvokeContext<'a, 'a>,
        seeds: &[&[u8]],
        address: &Pubkey,
    ) -> Result<Pubkey, Error> {
        let (address, _) = call_program_address_common(
            invoke_context,
            seeds,
            address,
            false,
            SyscallCreateProgramAddress::rust,
        )?;
        Ok(address)
    }

    fn try_find_program_address<'a>(
        invoke_context: &mut InvokeContext<'a, 'a>,
        seeds: &[&[u8]],
        address: &Pubkey,
    ) -> Result<(Pubkey, u8), Error> {
        call_program_address_common(
            invoke_context,
            seeds,
            address,
            false,
            SyscallTryFindProgramAddress::rust,
        )
    }

    #[test]
    fn test_set_and_get_return_data() {
        const SRC_VA: u64 = 0x100000000;
        const DST_VA: u64 = 0x200000000;
        const PROGRAM_ID_VA: u64 = 0x300000000;
        let data = vec![42; 24];
        let mut data_buffer = vec![0; 16];
        let mut id_buffer = vec![0; 32];

        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&data, SRC_VA),
                MemoryRegion::new_writable(&mut data_buffer, DST_VA),
                MemoryRegion::new_writable(&mut id_buffer, PROGRAM_ID_VA),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        let result = SyscallSetReturnData::rust(
            &mut invoke_context,
            SRC_VA,
            data.len() as u64,
            0,
            0,
            0,
            &mut memory_mapping,
        );
        assert_eq!(result.unwrap(), 0);

        let result = SyscallGetReturnData::rust(
            &mut invoke_context,
            DST_VA,
            data_buffer.len() as u64,
            PROGRAM_ID_VA,
            0,
            0,
            &mut memory_mapping,
        );
        assert_eq!(result.unwrap() as usize, data.len());
        assert_eq!(data.get(0..data_buffer.len()).unwrap(), data_buffer);
        assert_eq!(id_buffer, program_id.to_bytes());

        let result = SyscallGetReturnData::rust(
            &mut invoke_context,
            PROGRAM_ID_VA,
            data_buffer.len() as u64,
            PROGRAM_ID_VA,
            0,
            0,
            &mut memory_mapping,
        );
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::CopyOverlapping
        );
    }

    #[test]
    fn test_syscall_sol_get_processed_sibling_instruction() {
        let transaction_accounts = (0..9)
            .map(|_| {
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(0, 0, &bpf_loader::id()),
                )
            })
            .collect::<Vec<_>>();
        let instruction_trace = [1, 2, 3, 2, 2, 3, 4, 3];
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        for (index_in_trace, stack_height) in instruction_trace.into_iter().enumerate() {
            while stack_height
                <= invoke_context
                    .transaction_context
                    .get_instruction_stack_height()
            {
                invoke_context.transaction_context.pop().unwrap();
            }
            if stack_height
                > invoke_context
                    .transaction_context
                    .get_instruction_stack_height()
            {
                let instruction_accounts = vec![InstructionAccount::new(
                    index_in_trace.saturating_add(1) as IndexOfAccount,
                    false,
                    false,
                )];
                invoke_context
                    .transaction_context
                    .configure_next_instruction_for_tests(
                        0,
                        instruction_accounts,
                        vec![index_in_trace as u8],
                    )
                    .unwrap();
                invoke_context.transaction_context.push().unwrap();
            }
        }

        let syscall_base_cost = invoke_context.get_execution_cost().syscall_base_cost;

        const VM_BASE_ADDRESS: u64 = 0x100000000;
        const META_OFFSET: usize = 0;
        const PROGRAM_ID_OFFSET: usize =
            META_OFFSET + std::mem::size_of::<ProcessedSiblingInstruction>();
        const DATA_OFFSET: usize = PROGRAM_ID_OFFSET + std::mem::size_of::<Pubkey>();
        const ACCOUNTS_OFFSET: usize = DATA_OFFSET + 0x100;
        const END_OFFSET: usize = ACCOUNTS_OFFSET + std::mem::size_of::<AccountInfo>() * 4;
        let mut memory = [0u8; END_OFFSET];
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_writable(&mut memory, VM_BASE_ADDRESS)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        let processed_sibling_instruction =
            unsafe { &mut *memory.as_mut_ptr().cast::<ProcessedSiblingInstruction>() };
        processed_sibling_instruction.data_len = 1;
        processed_sibling_instruction.accounts_len = 1;

        invoke_context.mock_set_remaining(syscall_base_cost);
        let result = SyscallGetProcessedSiblingInstruction::rust(
            &mut invoke_context,
            0,
            VM_BASE_ADDRESS.saturating_add(META_OFFSET as u64),
            VM_BASE_ADDRESS.saturating_add(PROGRAM_ID_OFFSET as u64),
            VM_BASE_ADDRESS.saturating_add(DATA_OFFSET as u64),
            VM_BASE_ADDRESS.saturating_add(ACCOUNTS_OFFSET as u64),
            &mut memory_mapping,
        );
        assert_eq!(result.unwrap(), 1);
        {
            let program_id = translate_type::<Pubkey>(
                &memory_mapping,
                VM_BASE_ADDRESS.saturating_add(PROGRAM_ID_OFFSET as u64),
                true,
            )
            .unwrap();
            let data = translate_slice::<u8>(
                &memory_mapping,
                VM_BASE_ADDRESS.saturating_add(DATA_OFFSET as u64),
                processed_sibling_instruction.data_len,
                true,
            )
            .unwrap();
            let accounts = translate_slice::<AccountMeta>(
                &memory_mapping,
                VM_BASE_ADDRESS.saturating_add(ACCOUNTS_OFFSET as u64),
                processed_sibling_instruction.accounts_len,
                true,
            )
            .unwrap();
            let transaction_context = &invoke_context.transaction_context;
            assert_eq!(processed_sibling_instruction.data_len, 1);
            assert_eq!(processed_sibling_instruction.accounts_len, 1);
            assert_eq!(
                program_id,
                transaction_context.get_key_of_account_at_index(0).unwrap(),
            );
            assert_eq!(data, &[5]);
            assert_eq!(
                accounts,
                &[AccountMeta {
                    pubkey: *transaction_context.get_key_of_account_at_index(6).unwrap(),
                    is_signer: false,
                    is_writable: false
                }]
            );
        }

        invoke_context.mock_set_remaining(syscall_base_cost);
        let result = SyscallGetProcessedSiblingInstruction::rust(
            &mut invoke_context,
            1,
            VM_BASE_ADDRESS.saturating_add(META_OFFSET as u64),
            VM_BASE_ADDRESS.saturating_add(PROGRAM_ID_OFFSET as u64),
            VM_BASE_ADDRESS.saturating_add(DATA_OFFSET as u64),
            VM_BASE_ADDRESS.saturating_add(ACCOUNTS_OFFSET as u64),
            &mut memory_mapping,
        );
        assert_eq!(result.unwrap(), 0);

        invoke_context.mock_set_remaining(syscall_base_cost);
        let result = SyscallGetProcessedSiblingInstruction::rust(
            &mut invoke_context,
            0,
            VM_BASE_ADDRESS.saturating_add(META_OFFSET as u64),
            VM_BASE_ADDRESS.saturating_add(META_OFFSET as u64),
            VM_BASE_ADDRESS.saturating_add(META_OFFSET as u64),
            VM_BASE_ADDRESS.saturating_add(META_OFFSET as u64),
            &mut memory_mapping,
        );
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::CopyOverlapping
        );
    }

    #[test]
    fn test_create_program_address() {
        // These tests duplicate the direct tests in solana_pubkey

        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let address = bpf_loader_upgradeable::id();

        let exceeded_seed = &[127; MAX_SEED_LEN + 1];
        assert_matches!(
            create_program_address(&mut invoke_context, &[exceeded_seed], &address),
            Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded)
        );
        assert_matches!(
            create_program_address(
                &mut invoke_context,
                &[b"short_seed", exceeded_seed],
                &address,
            ),
            Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded)
        );
        let max_seed = &[0; MAX_SEED_LEN];
        assert!(create_program_address(&mut invoke_context, &[max_seed], &address).is_ok());
        let exceeded_seeds: &[&[u8]] = &[
            &[1],
            &[2],
            &[3],
            &[4],
            &[5],
            &[6],
            &[7],
            &[8],
            &[9],
            &[10],
            &[11],
            &[12],
            &[13],
            &[14],
            &[15],
            &[16],
        ];
        assert!(create_program_address(&mut invoke_context, exceeded_seeds, &address).is_ok());
        let max_seeds: &[&[u8]] = &[
            &[1],
            &[2],
            &[3],
            &[4],
            &[5],
            &[6],
            &[7],
            &[8],
            &[9],
            &[10],
            &[11],
            &[12],
            &[13],
            &[14],
            &[15],
            &[16],
            &[17],
        ];
        assert_matches!(
            create_program_address(&mut invoke_context, max_seeds, &address),
            Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded)
        );
        assert_eq!(
            create_program_address(&mut invoke_context, &[b"", &[1]], &address).unwrap(),
            "BwqrghZA2htAcqq8dzP1WDAhTXYTYWj7CHxF5j7TDBAe"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            create_program_address(&mut invoke_context, &["☉".as_ref(), &[0]], &address).unwrap(),
            "13yWmRpaTR4r5nAktwLqMpRNr28tnVUZw26rTvPSSB19"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            create_program_address(&mut invoke_context, &[b"Talking", b"Squirrels"], &address)
                .unwrap(),
            "2fnQrngrQT4SeLcdToJAD96phoEjNL2man2kfRLCASVk"
                .parse()
                .unwrap(),
        );
        let public_key = Pubkey::from_str("SeedPubey1111111111111111111111111111111111").unwrap();
        assert_eq!(
            create_program_address(&mut invoke_context, &[public_key.as_ref(), &[1]], &address)
                .unwrap(),
            "976ymqVnfE32QFe6NfGDctSvVa36LWnvYxhU6G2232YL"
                .parse()
                .unwrap(),
        );
        assert_ne!(
            create_program_address(&mut invoke_context, &[b"Talking", b"Squirrels"], &address)
                .unwrap(),
            create_program_address(&mut invoke_context, &[b"Talking"], &address).unwrap(),
        );
        invoke_context.mock_set_remaining(0);
        assert_matches!(
            create_program_address(&mut invoke_context, &[b"", &[1]], &address),
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );
    }

    #[test]
    fn test_find_program_address() {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let cost = invoke_context
            .get_execution_cost()
            .create_program_address_units;
        let address = bpf_loader_upgradeable::id();
        let max_tries = 256; // one per seed

        for _ in 0..1_000 {
            let address = Pubkey::new_unique();
            invoke_context.mock_set_remaining(cost * max_tries);
            let (found_address, bump_seed) =
                try_find_program_address(&mut invoke_context, &[b"Lil'", b"Bits"], &address)
                    .unwrap();
            assert_eq!(
                found_address,
                create_program_address(
                    &mut invoke_context,
                    &[b"Lil'", b"Bits", &[bump_seed]],
                    &address,
                )
                .unwrap()
            );
        }

        let seeds: &[&[u8]] = &[b""];
        invoke_context.mock_set_remaining(cost * max_tries);
        let (_, bump_seed) =
            try_find_program_address(&mut invoke_context, seeds, &address).unwrap();
        invoke_context.mock_set_remaining(cost * (max_tries - bump_seed as u64));
        try_find_program_address(&mut invoke_context, seeds, &address).unwrap();
        invoke_context.mock_set_remaining(cost * (max_tries - bump_seed as u64 - 1));
        assert_matches!(
            try_find_program_address(&mut invoke_context, seeds, &address),
            Result::Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ComputationalBudgetExceeded
        );

        let exceeded_seed = &[127; MAX_SEED_LEN + 1];
        invoke_context.mock_set_remaining(cost * (max_tries - 1));
        assert_matches!(
            try_find_program_address(&mut invoke_context, &[exceeded_seed], &address),
            Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded)
        );
        let exceeded_seeds: &[&[u8]] = &[
            &[1],
            &[2],
            &[3],
            &[4],
            &[5],
            &[6],
            &[7],
            &[8],
            &[9],
            &[10],
            &[11],
            &[12],
            &[13],
            &[14],
            &[15],
            &[16],
            &[17],
        ];
        invoke_context.mock_set_remaining(cost * (max_tries - 1));
        assert_matches!(
            try_find_program_address(&mut invoke_context, exceeded_seeds, &address),
            Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::BadSeeds(PubkeyError::MaxSeedLengthExceeded)
        );

        assert_matches!(
            call_program_address_common(
                &mut invoke_context,
                seeds,
                &address,
                true,
                SyscallTryFindProgramAddress::rust,
            ),
            Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::CopyOverlapping
        );
    }

    #[test]
    fn test_syscall_big_mod_exp() {
        let config = Config::default();
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());

        const VADDR_PARAMS: u64 = 0x100000000;
        const MAX_LEN: u64 = 512;
        const INV_LEN: u64 = MAX_LEN + 1;
        let data: [u8; INV_LEN as usize] = [0; INV_LEN as usize];
        const VADDR_DATA: u64 = 0x200000000;

        let mut data_out: [u8; INV_LEN as usize] = [0; INV_LEN as usize];
        const VADDR_OUT: u64 = 0x300000000;

        // Test that SyscallBigModExp succeeds with the maximum param size
        {
            let params_max_len = BigModExpParams {
                base: VADDR_DATA as *const u8,
                base_len: MAX_LEN,
                exponent: VADDR_DATA as *const u8,
                exponent_len: MAX_LEN,
                modulus: VADDR_DATA as *const u8,
                modulus_len: MAX_LEN,
            };

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    MemoryRegion::new_readonly(bytes_of(&params_max_len), VADDR_PARAMS),
                    MemoryRegion::new_readonly(&data, VADDR_DATA),
                    MemoryRegion::new_writable(&mut data_out, VADDR_OUT),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let budget = invoke_context.get_execution_cost();
            invoke_context.mock_set_remaining(
                budget.syscall_base_cost
                    + (MAX_LEN * MAX_LEN) / budget.big_modular_exponentiation_cost_divisor
                    + budget.big_modular_exponentiation_base_cost,
            );

            let result = SyscallBigModExp::rust(
                &mut invoke_context,
                VADDR_PARAMS,
                VADDR_OUT,
                0,
                0,
                0,
                &mut memory_mapping,
            );

            assert_eq!(result.unwrap(), 0);
        }

        // Test that SyscallBigModExp fails when the maximum param size is exceeded
        {
            let params_inv_len = BigModExpParams {
                base: VADDR_DATA as *const u8,
                base_len: INV_LEN,
                exponent: VADDR_DATA as *const u8,
                exponent_len: INV_LEN,
                modulus: VADDR_DATA as *const u8,
                modulus_len: INV_LEN,
            };

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    MemoryRegion::new_readonly(bytes_of(&params_inv_len), VADDR_PARAMS),
                    MemoryRegion::new_readonly(&data, VADDR_DATA),
                    MemoryRegion::new_writable(&mut data_out, VADDR_OUT),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let budget = invoke_context.get_execution_cost();
            invoke_context.mock_set_remaining(
                budget.syscall_base_cost
                    + (INV_LEN * INV_LEN) / budget.big_modular_exponentiation_cost_divisor
                    + budget.big_modular_exponentiation_base_cost,
            );

            let result = SyscallBigModExp::rust(
                &mut invoke_context,
                VADDR_PARAMS,
                VADDR_OUT,
                0,
                0,
                0,
                &mut memory_mapping,
            );

            assert_matches!(
                result,
                Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::InvalidLength
            );
        }
    }

    #[test]
    fn test_syscall_get_epoch_stake_total_stake() {
        let config = Config::default();
        let compute_cost = SVMTransactionExecutionCost::default();
        let mut compute_budget = SVMTransactionExecutionBudget::default();
        let sysvar_cache = Arc::<SysvarCache>::default();

        const EXPECTED_TOTAL_STAKE: u64 = 200_000_000_000_000;

        struct MockCallback {}
        impl InvokeContextCallback for MockCallback {
            fn get_epoch_stake(&self) -> u64 {
                EXPECTED_TOTAL_STAKE
            }
            // Vote accounts are not needed for this test.
        }

        // Compute units, as specified by SIMD-0133.
        // cu = syscall_base_cost
        let expected_cus = compute_cost.syscall_base_cost;

        // Set the compute budget to the expected CUs to ensure the syscall
        // doesn't exceed the expected usage.
        compute_budget.compute_unit_limit = expected_cus;

        with_mock_invoke_context!(invoke_context, transaction_context, vec![]);
        let feature_set = SVMFeatureSet::default();
        let program_runtime_environments = ProgramRuntimeEnvironments::default();
        invoke_context.environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockCallback {},
            &feature_set,
            &program_runtime_environments,
            &program_runtime_environments,
            &sysvar_cache,
        );

        let null_pointer_var = std::ptr::null::<Pubkey>() as u64;

        let mut memory_mapping = MemoryMapping::new(vec![], &config, SBPFVersion::V3).unwrap();

        let result = SyscallGetEpochStake::rust(
            &mut invoke_context,
            null_pointer_var,
            0,
            0,
            0,
            0,
            &mut memory_mapping,
        )
        .unwrap();

        assert_eq!(result, EXPECTED_TOTAL_STAKE);
    }

    #[test]
    fn test_syscall_get_epoch_stake_vote_account_stake() {
        let config = Config::default();
        let mut compute_budget = SVMTransactionExecutionBudget::default();
        let compute_cost = SVMTransactionExecutionCost::default();
        let sysvar_cache = Arc::<SysvarCache>::default();

        const TARGET_VOTE_ADDRESS: Pubkey = Pubkey::new_from_array([2; 32]);
        const EXPECTED_EPOCH_STAKE: u64 = 55_000_000_000;

        struct MockCallback {}
        impl InvokeContextCallback for MockCallback {
            // Total stake is not needed for this test.
            fn get_epoch_stake_for_vote_account(&self, vote_address: &Pubkey) -> u64 {
                if *vote_address == TARGET_VOTE_ADDRESS {
                    EXPECTED_EPOCH_STAKE
                } else {
                    0
                }
            }
        }

        // Compute units, as specified by SIMD-0133.
        // cu = syscall_base_cost
        //     + floor(32/cpi_bytes_per_unit)
        //     + mem_op_base_cost
        let expected_cus = compute_cost.syscall_base_cost
            + (PUBKEY_BYTES as u64) / compute_cost.cpi_bytes_per_unit
            + compute_cost.mem_op_base_cost;

        // Set the compute budget to the expected CUs to ensure the syscall
        // doesn't exceed the expected usage.
        compute_budget.compute_unit_limit = expected_cus;

        with_mock_invoke_context!(invoke_context, transaction_context, vec![]);
        let feature_set = SVMFeatureSet::default();
        let program_runtime_environments = ProgramRuntimeEnvironments::default();
        invoke_context.environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockCallback {},
            &feature_set,
            &program_runtime_environments,
            &program_runtime_environments,
            &sysvar_cache,
        );

        {
            // The syscall aborts the virtual machine if not all bytes in VM
            // memory range `[vote_addr, vote_addr + 32)` are readable.
            let vote_address_var = 0x100000000;

            let mut memory_mapping = MemoryMapping::new(
                vec![
                    // Invalid read-only memory region.
                    MemoryRegion::new_readonly(&[2; 31], vote_address_var),
                ],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetEpochStake::rust(
                &mut invoke_context,
                vote_address_var,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            );

            assert_access_violation!(result, vote_address_var, 32);
        }

        invoke_context.mock_set_remaining(compute_budget.compute_unit_limit);
        {
            // Otherwise, the syscall returns a `u64` integer representing the
            // total active stake delegated to the vote account at the provided
            // address.
            let vote_address_var = 0x100000000;

            let mut memory_mapping = MemoryMapping::new(
                vec![MemoryRegion::new_readonly(
                    bytes_of(&TARGET_VOTE_ADDRESS),
                    vote_address_var,
                )],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetEpochStake::rust(
                &mut invoke_context,
                vote_address_var,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            )
            .unwrap();

            assert_eq!(result, EXPECTED_EPOCH_STAKE);
        }

        invoke_context.mock_set_remaining(compute_budget.compute_unit_limit);
        {
            // If the provided vote address corresponds to an account that is
            // not a vote account or does not exist, the syscall will write
            // `0` for active stake.
            let vote_address_var = 0x100000000;
            let not_a_vote_address = Pubkey::new_unique(); // Not a vote account.

            let mut memory_mapping = MemoryMapping::new(
                vec![MemoryRegion::new_readonly(
                    bytes_of(&not_a_vote_address),
                    vote_address_var,
                )],
                &config,
                SBPFVersion::V3,
            )
            .unwrap();

            let result = SyscallGetEpochStake::rust(
                &mut invoke_context,
                vote_address_var,
                0,
                0,
                0,
                0,
                &mut memory_mapping,
            )
            .unwrap();

            assert_eq!(result, 0); // `0` for active stake.
        }
    }

    #[test]
    fn test_check_type_assumptions() {
        check_type_assumptions();
    }

    fn bytes_of<T>(val: &T) -> &[u8] {
        let size = mem::size_of::<T>();
        unsafe { slice::from_raw_parts(std::slice::from_ref(val).as_ptr().cast(), size) }
    }

    fn bytes_of_mut<T>(val: &mut T) -> &mut [u8] {
        let size = mem::size_of::<T>();
        unsafe { slice::from_raw_parts_mut(slice::from_mut(val).as_mut_ptr().cast(), size) }
    }

    fn bytes_of_slice<T>(val: &[T]) -> &[u8] {
        let size = val.len().wrapping_mul(mem::size_of::<T>());
        unsafe { slice::from_raw_parts(val.as_ptr().cast(), size) }
    }

    fn bytes_of_slice_mut<T>(val: &mut [T]) -> &mut [u8] {
        let size = val.len().wrapping_mul(mem::size_of::<T>());
        unsafe { slice::from_raw_parts_mut(val.as_mut_ptr().cast(), size) }
    }

    #[test]
    fn test_address_is_aligned() {
        for address in 0..std::mem::size_of::<u64>() {
            assert_eq!(address_is_aligned::<u64>(address as u64), address == 0);
        }
    }

    #[test_case(0x100000004, 0x100000004, &[0x00, 0x00, 0x00, 0x00])] // Intra region match
    #[test_case(0x100000003, 0x100000004, &[0xFF, 0xFF, 0xFF, 0xFF])] // Intra region down
    #[test_case(0x100000005, 0x100000004, &[0x01, 0x00, 0x00, 0x00])] // Intra region up
    #[test_case(0x100000004, 0x200000004, &[0x00, 0x00, 0x00, 0x00])] // Inter region match
    #[test_case(0x100000003, 0x200000004, &[0xFF, 0xFF, 0xFF, 0xFF])] // Inter region down
    #[test_case(0x100000005, 0x200000004, &[0x01, 0x00, 0x00, 0x00])] // Inter region up
    fn test_memcmp_success(src_a: u64, src_b: u64, expected_result: &[u8; 4]) {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let mem = (0..12).collect::<Vec<u8>>();
        let mut result_mem = vec![0; 4];
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mem, 0x100000000),
                MemoryRegion::new_readonly(&mem, 0x200000000),
                MemoryRegion::new_writable(&mut result_mem, 0x300000000),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let result = SyscallMemcmp::rust(
            &mut invoke_context,
            src_a,
            src_b,
            4,
            0x300000000,
            0,
            &mut memory_mapping,
        );
        result.unwrap();
        assert_eq!(result_mem, expected_result);
    }

    #[test_case(0x100000002, 0x100000004, 18245498089483734664)] // Down overlapping
    #[test_case(0x100000004, 0x100000002, 6092969436446403628)] // Up overlapping
    #[test_case(0x100000002, 0x100000006, 16598193894146733116)] // Down touching
    #[test_case(0x100000006, 0x100000002, 8940776276357560353)] // Up touching
    #[test_case(0x100000000, 0x100000008, 1288053912680171784)] // Down apart
    #[test_case(0x100000008, 0x100000000, 4652742827052033592)] // Up apart
    #[test_case(0x100000004, 0x200000004, 8833460765081683332)] // Down inter region
    #[test_case(0x200000004, 0x100000004, 11837649335115988407)] // Up inter region
    fn test_memmove_success(dst: u64, src: u64, expected_hash: u64) {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let mut mem = (0..24).collect::<Vec<u8>>();
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_writable(&mut mem[..12], 0x100000000),
                MemoryRegion::new_writable(&mut mem[12..], 0x200000000),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let result =
            SyscallMemmove::rust(&mut invoke_context, dst, src, 4, 0, 0, &mut memory_mapping);
        result.unwrap();
        let mut hasher = DefaultHasher::new();
        mem.hash(&mut hasher);
        assert_eq!(hasher.finish(), expected_hash);
    }

    #[test_case(0x100000002, 0x00, 6070675560359421890)]
    #[test_case(0x100000002, 0xFF, 3413209638111181029)]
    fn test_memset_success(dst: u64, value: u64, expected_hash: u64) {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let mut mem = (0..12).collect::<Vec<u8>>();
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_writable(&mut mem, 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let result = SyscallMemset::rust(
            &mut invoke_context,
            dst,
            value,
            4,
            0,
            0,
            &mut memory_mapping,
        );
        result.unwrap();
        let mut hasher = DefaultHasher::new();
        mem.hash(&mut hasher);
        assert_eq!(hasher.finish(), expected_hash);
    }

    #[test_case(0x100000002, 0x100000004)] // Down overlapping
    #[test_case(0x100000004, 0x100000002)] // Up overlapping
    fn test_memcpy_overlapping(dst: u64, src: u64) {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let mut mem = (0..12).collect::<Vec<u8>>();
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_writable(&mut mem, 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let result =
            SyscallMemcpy::rust(&mut invoke_context, dst, src, 4, 0, 0, &mut memory_mapping);
        assert_matches!(
            result,
            Result::Err(error) if error.downcast_ref::<SyscallError>().unwrap() == &SyscallError::CopyOverlapping
        );
    }

    #[test_case(0xFFFFFFFFF, 0x100000006, 0xFFFFFFFFF)] // Dst lower bound
    #[test_case(0x100000010, 0x100000006, 0x100000010)] // Dst upper bound
    #[test_case(0x100000002, 0xFFFFFFFFF, 0xFFFFFFFFF)] // Src lower bound
    #[test_case(0x100000002, 0x100000010, 0x100000010)] // Src upper bound
    fn test_memops_access_violation(dst: u64, src: u64, fault_address: u64) {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let mut mem = (0..12).collect::<Vec<u8>>();
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_writable(&mut mem, 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let result =
            SyscallMemcpy::rust(&mut invoke_context, dst, src, 4, 0, 0, &mut memory_mapping);
        assert_access_violation!(result, fault_address, 4);
        let result =
            SyscallMemmove::rust(&mut invoke_context, dst, src, 4, 0, 0, &mut memory_mapping);
        assert_access_violation!(result, fault_address, 4);
        let result =
            SyscallMemcmp::rust(&mut invoke_context, dst, src, 4, 0, 0, &mut memory_mapping);
        assert_access_violation!(result, fault_address, 4);
    }

    #[test_case(0xFFFFFFFFF)] // Dst lower bound
    #[test_case(0x100000010)] // Dst upper bound
    fn test_memset_access_violation(dst: u64) {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let mut mem = (0..12).collect::<Vec<u8>>();
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_writable(&mut mem, 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let result = SyscallMemset::rust(&mut invoke_context, dst, 0, 4, 0, 0, &mut memory_mapping);
        assert_access_violation!(result, dst, 4);
    }

    #[test]
    fn test_memcmp_result_access_violation() {
        prepare_mockup!(invoke_context, program_id, bpf_loader::id());
        let mem = (0..12).collect::<Vec<u8>>();
        let config = Config::default();
        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(&mem, 0x100000000)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let result = SyscallMemcmp::rust(
            &mut invoke_context,
            0x100000000,
            0x100000000,
            4,
            0x100000000,
            0,
            &mut memory_mapping,
        );
        assert_access_violation!(result, 0x100000000, 4);
    }
}
