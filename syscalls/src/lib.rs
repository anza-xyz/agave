#![cfg(feature = "agave-unstable-api")]
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

// NOTE: These constants are temporarily defined here and will be
// moved to a dedicated crate in the future.
mod bls12_381_curve_id {
    /// Curve ID for BLS12-381 pairing operations
    pub(crate) const BLS12_381_LE: u64 = 4;
    pub(crate) const BLS12_381_BE: u64 = 4 | 0x80;

    /// Curve ID for BLS12-381 G1 group operations
    pub(crate) const BLS12_381_G1_LE: u64 = 5;
    pub(crate) const BLS12_381_G1_BE: u64 = 5 | 0x80;

    /// Curve ID for BLS12-381 G2 group operations
    pub(crate) const BLS12_381_G2_LE: u64 = 6;
    pub(crate) const BLS12_381_G2_BE: u64 = 6 | 0x80;
}

fn consume_compute_meter(invoke_context: &InvokeContext, amount: u64) -> Result<(), Error> {
    invoke_context.consume_checked(amount)?;
    Ok(())
}

// NOTE: This macro name is checked by gen-syscall-list to create the list of
// syscalls. If this macro name is changed, or if a new one is added, then
// gen-syscall-list/build.rs must also be updated.
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
    let enable_bls12_381_syscall = feature_set.enable_bls12_381_syscall;
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
        allow_memory_region_zero: feature_set.enable_sbpf_v3_deployment_and_execution,
        // Warning, do not use `Config::default()` so that configuration here is explicit.
    };

    // NOTE: `register_function` calls are checked by gen-syscall-list to create
    // the list of syscalls. If this function name is changed, or if a new one
    // is added, then gen-syscall-list/build.rs must also be updated.
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
    register_feature_gated_function!(
        result,
        enable_bls12_381_syscall,
        "sol_curve_decompress",
        SyscallCurveDecompress::vm,
    )?;
    register_feature_gated_function!(
        result,
        enable_bls12_381_syscall,
        "sol_curve_pairing_map",
        SyscallCurvePairingMap::vm,
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
        allow_memory_region_zero: true,
        // Warning, do not use `Config::default()` so that configuration here is explicit.
    };
    BuiltinProgram::new_loader(config)
}

fn translate_type<T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    check_aligned: bool,
) -> Result<&T, Error> {
    translate_type_inner!(memory_mapping, AccessType::Load, vm_addr, T, check_aligned)
        .map(|value| &*value)
}
fn translate_slice<T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    len: u64,
    check_aligned: bool,
) -> Result<&[T], Error> {
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
fn translate_type_mut<T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    check_aligned: bool,
) -> Result<&mut T, Error> {
    translate_type_inner!(memory_mapping, AccessType::Store, vm_addr, T, check_aligned)
}
// Do not use this directly
#[allow(clippy::mut_from_ref)]
fn translate_slice_mut<T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    len: u64,
    check_aligned: bool,
) -> Result<&mut [T], Error> {
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

fn translate_and_check_program_address_inputs(
    seeds_addr: u64,
    seeds_len: u64,
    program_id_addr: u64,
    memory_mapping: &mut MemoryMapping,
    check_aligned: bool,
) -> Result<(Vec<&[u8]>, &Pubkey), Error> {
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
    // Currently, the following curves are supported:
    // - Curve25519 Edwards and Ristretto representations
    // - BLS12-381
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
        use {
            crate::bls12_381_curve_id::*,
            solana_curve25519::{curve_syscall_traits::*, edwards, ristretto},
        };

        // SIMD-0388: BLS12-381 syscalls
        if !invoke_context.get_feature_set().enable_bls12_381_syscall
            && matches!(
                curve_id,
                BLS12_381_G1_BE | BLS12_381_G1_LE | BLS12_381_G2_BE | BLS12_381_G2_LE
            )
        {
            return Err(SyscallError::InvalidAttribute.into());
        }

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
            BLS12_381_G1_LE | BLS12_381_G1_BE => {
                let cost = invoke_context
                    .get_execution_cost()
                    .bls12_381_g1_validate_cost;
                consume_compute_meter(invoke_context, cost)?;

                let point = translate_type::<agave_bls12_381::PodG1Point>(
                    memory_mapping,
                    point_addr,
                    invoke_context.get_check_aligned(),
                )?;

                let endianness = if curve_id == BLS12_381_G1_LE {
                    agave_bls12_381::Endianness::LE
                } else {
                    agave_bls12_381::Endianness::BE
                };

                if agave_bls12_381::bls12_381_g1_point_validation(
                    agave_bls12_381::Version::V0,
                    point,
                    endianness,
                ) {
                    Ok(SUCCESS)
                } else {
                    Ok(1)
                }
            }
            BLS12_381_G2_LE | BLS12_381_G2_BE => {
                let cost = invoke_context
                    .get_execution_cost()
                    .bls12_381_g2_validate_cost;
                consume_compute_meter(invoke_context, cost)?;

                let point = translate_type::<agave_bls12_381::PodG2Point>(
                    memory_mapping,
                    point_addr,
                    invoke_context.get_check_aligned(),
                )?;

                let endianness = if curve_id == BLS12_381_G2_LE {
                    agave_bls12_381::Endianness::LE
                } else {
                    agave_bls12_381::Endianness::BE
                };

                if agave_bls12_381::bls12_381_g2_point_validation(
                    agave_bls12_381::Version::V0,
                    point,
                    endianness,
                ) {
                    Ok(SUCCESS)
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
    // Elliptic Curve Point Decompression
    //
    // Currently, the following curves are supported:
    // - BLS12-381
    SyscallCurveDecompress,
    fn rust(
        invoke_context: &mut InvokeContext,
        curve_id: u64,
        point_addr: u64,
        result_addr: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        use {
            crate::bls12_381_curve_id::*,
            agave_bls12_381::{
                PodG1Compressed as PodBLSG1Compressed, PodG1Point as PodBLSG1Point,
                PodG2Compressed as PodBLSG2Compressed, PodG2Point as PodBLSG2Point,
            },
        };

        match curve_id {
            BLS12_381_G1_LE | BLS12_381_G1_BE => {
                let cost = invoke_context
                    .get_execution_cost()
                    .bls12_381_g1_decompress_cost;
                consume_compute_meter(invoke_context, cost)?;

                let compressed_point = translate_type::<PodBLSG1Compressed>(
                    memory_mapping,
                    point_addr,
                    invoke_context.get_check_aligned(),
                )?;

                let endianness = if curve_id == BLS12_381_G1_LE {
                    agave_bls12_381::Endianness::LE
                } else {
                    agave_bls12_381::Endianness::BE
                };

                if let Some(affine_point) = agave_bls12_381::bls12_381_g1_decompress(
                    agave_bls12_381::Version::V0,
                    compressed_point,
                    endianness,
                ) {
                    translate_mut!(
                        memory_mapping,
                        invoke_context.get_check_aligned(),
                        let result_ref_mut: &mut PodBLSG1Point = map(result_addr)?;
                    );
                    *result_ref_mut = affine_point;
                    Ok(SUCCESS)
                } else {
                    Ok(1)
                }
            }
            BLS12_381_G2_LE | BLS12_381_G2_BE => {
                let cost = invoke_context
                    .get_execution_cost()
                    .bls12_381_g2_decompress_cost;
                consume_compute_meter(invoke_context, cost)?;

                let compressed_point = translate_type::<PodBLSG2Compressed>(
                    memory_mapping,
                    point_addr,
                    invoke_context.get_check_aligned(),
                )?;

                let endianness = if curve_id == BLS12_381_G2_LE {
                    agave_bls12_381::Endianness::LE
                } else {
                    agave_bls12_381::Endianness::BE
                };

                if let Some(affine_point) = agave_bls12_381::bls12_381_g2_decompress(
                    agave_bls12_381::Version::V0,
                    compressed_point,
                    endianness,
                ) {
                    translate_mut!(
                        memory_mapping,
                        invoke_context.get_check_aligned(),
                        let result_ref_mut: &mut PodBLSG2Point = map(result_addr)?;
                    );
                    *result_ref_mut = affine_point;
                    Ok(SUCCESS)
                } else {
                    Ok(1)
                }
            }
            _ => Err(SyscallError::InvalidAttribute.into()),
        }
    }
);

declare_builtin_function!(
    // Elliptic Curve Group Operations
    //
    // Currently, the following curves are supported:
    // - Curve25519 Edwards and Ristretto representations
    // - BLS12-381
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
        use {
            crate::bls12_381_curve_id::*,
            agave_bls12_381::{
                PodG1Point as PodBLSG1Point, PodG2Point as PodBLSG2Point, PodScalar as PodBLSScalar,
            },
            solana_curve25519::{
                curve_syscall_traits::*,
                edwards::{self, PodEdwardsPoint},
                ristretto::{self, PodRistrettoPoint},
                scalar,
            },
        };

        if !invoke_context.get_feature_set().enable_bls12_381_syscall
            && matches!(
                curve_id,
                BLS12_381_G1_BE | BLS12_381_G1_LE | BLS12_381_G2_BE | BLS12_381_G2_LE
            )
        {
            return Err(SyscallError::InvalidAttribute.into());
        }

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

            BLS12_381_G1_LE | BLS12_381_G1_BE => {
                let endianness = if curve_id == BLS12_381_G1_LE {
                    agave_bls12_381::Endianness::LE
                } else {
                    agave_bls12_381::Endianness::BE
                };

                match group_op {
                    ADD => {
                        let cost = invoke_context.get_execution_cost().bls12_381_g1_add_cost;
                        consume_compute_meter(invoke_context, cost)?;

                        let left_point = translate_type::<PodBLSG1Point>(
                            memory_mapping,
                            left_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;
                        let right_point = translate_type::<PodBLSG1Point>(
                            memory_mapping,
                            right_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;

                        if let Some(result_point) = agave_bls12_381::bls12_381_g1_addition(
                            agave_bls12_381::Version::V0,
                            left_point,
                            right_point,
                            endianness,
                        ) {
                            translate_mut!(
                                memory_mapping,
                                invoke_context.get_check_aligned(),
                                let result_point_ref_mut: &mut PodBLSG1Point = map(result_point_addr)?;
                            );
                            *result_point_ref_mut = result_point;
                            Ok(SUCCESS)
                        } else {
                            Ok(1)
                        }
                    }
                    SUB => {
                        let cost = invoke_context
                            .get_execution_cost()
                            .bls12_381_g1_subtract_cost;
                        consume_compute_meter(invoke_context, cost)?;

                        let left_point = translate_type::<PodBLSG1Point>(
                            memory_mapping,
                            left_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;
                        let right_point = translate_type::<PodBLSG1Point>(
                            memory_mapping,
                            right_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;

                        if let Some(result_point) = agave_bls12_381::bls12_381_g1_subtraction(
                            agave_bls12_381::Version::V0,
                            left_point,
                            right_point,
                            endianness,
                        ) {
                            translate_mut!(
                                memory_mapping,
                                invoke_context.get_check_aligned(),
                                let result_point_ref_mut: &mut PodBLSG1Point = map(result_point_addr)?;
                            );
                            *result_point_ref_mut = result_point;
                            Ok(SUCCESS)
                        } else {
                            Ok(1)
                        }
                    }
                    MUL => {
                        let cost = invoke_context
                            .get_execution_cost()
                            .bls12_381_g1_multiply_cost;
                        consume_compute_meter(invoke_context, cost)?;

                        let scalar = translate_type::<PodBLSScalar>(
                            memory_mapping,
                            left_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;
                        let point = translate_type::<PodBLSG1Point>(
                            memory_mapping,
                            right_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;

                        if let Some(result_point) = agave_bls12_381::bls12_381_g1_multiplication(
                            agave_bls12_381::Version::V0,
                            point,
                            scalar,
                            endianness,
                        ) {
                            translate_mut!(
                                memory_mapping,
                                invoke_context.get_check_aligned(),
                                let result_point_ref_mut: &mut PodBLSG1Point = map(result_point_addr)?;
                            );
                            *result_point_ref_mut = result_point;
                            Ok(SUCCESS)
                        } else {
                            Ok(1)
                        }
                    }
                    _ => Err(SyscallError::InvalidAttribute.into()),
                }
            }

            // New BLS12-381 G2 Implementation
            BLS12_381_G2_LE | BLS12_381_G2_BE => {
                let endianness = if curve_id == BLS12_381_G2_LE {
                    agave_bls12_381::Endianness::LE
                } else {
                    agave_bls12_381::Endianness::BE
                };

                match group_op {
                    ADD => {
                        let cost = invoke_context.get_execution_cost().bls12_381_g2_add_cost;
                        consume_compute_meter(invoke_context, cost)?;

                        let left_point = translate_type::<PodBLSG2Point>(
                            memory_mapping,
                            left_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;
                        let right_point = translate_type::<PodBLSG2Point>(
                            memory_mapping,
                            right_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;

                        if let Some(result_point) = agave_bls12_381::bls12_381_g2_addition(
                            agave_bls12_381::Version::V0,
                            left_point,
                            right_point,
                            endianness,
                        ) {
                            translate_mut!(
                                memory_mapping,
                                invoke_context.get_check_aligned(),
                                let result_point_ref_mut: &mut PodBLSG2Point = map(result_point_addr)?;
                            );
                            *result_point_ref_mut = result_point;
                            Ok(SUCCESS)
                        } else {
                            Ok(1)
                        }
                    }
                    SUB => {
                        let cost = invoke_context
                            .get_execution_cost()
                            .bls12_381_g2_subtract_cost;
                        consume_compute_meter(invoke_context, cost)?;

                        let left_point = translate_type::<PodBLSG2Point>(
                            memory_mapping,
                            left_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;
                        let right_point = translate_type::<PodBLSG2Point>(
                            memory_mapping,
                            right_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;

                        if let Some(result_point) = agave_bls12_381::bls12_381_g2_subtraction(
                            agave_bls12_381::Version::V0,
                            left_point,
                            right_point,
                            endianness,
                        ) {
                            translate_mut!(
                                memory_mapping,
                                invoke_context.get_check_aligned(),
                                let result_point_ref_mut: &mut PodBLSG2Point = map(result_point_addr)?;
                            );
                            *result_point_ref_mut = result_point;
                            Ok(SUCCESS)
                        } else {
                            Ok(1)
                        }
                    }
                    MUL => {
                        let cost = invoke_context
                            .get_execution_cost()
                            .bls12_381_g2_multiply_cost;
                        consume_compute_meter(invoke_context, cost)?;

                        let scalar = translate_type::<PodBLSScalar>(
                            memory_mapping,
                            left_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;
                        let point = translate_type::<PodBLSG2Point>(
                            memory_mapping,
                            right_input_addr,
                            invoke_context.get_check_aligned(),
                        )?;

                        if let Some(result_point) = agave_bls12_381::bls12_381_g2_multiplication(
                            agave_bls12_381::Version::V0,
                            point,
                            scalar,
                            endianness,
                        ) {
                            translate_mut!(
                                memory_mapping,
                                invoke_context.get_check_aligned(),
                                let result_point_ref_mut: &mut PodBLSG2Point = map(result_point_addr)?;
                            );
                            *result_point_ref_mut = result_point;
                            Ok(SUCCESS)
                        } else {
                            Ok(1)
                        }
                    }
                    _ => Err(SyscallError::InvalidAttribute.into()),
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
    // Elliptic Curve Multiscalar Multiplication
    //
    // Currently, the following curves are supported:
    // - Curve25519 Edwards and Ristretto representations
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
    /// Elliptic Curve Pairing Map
    ///
    // Currently, the following curves are supported:
    // - BLS12-381
    SyscallCurvePairingMap,
    fn rust(
        invoke_context: &mut InvokeContext,
        curve_id: u64,
        num_pairs: u64,
        g1_points_addr: u64,
        g2_points_addr: u64,
        result_addr: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        use {
            crate::bls12_381_curve_id::*,
            agave_bls12_381::{
                PodG1Point as PodBLSG1Point, PodG2Point as PodBLSG2Point,
                PodGtElement as PodBLSGtElement,
            },
        };

        match curve_id {
            BLS12_381_LE | BLS12_381_BE => {
                let execution_cost = invoke_context.get_execution_cost();
                let cost = execution_cost
                    .bls12_381_one_pair_cost
                    .saturating_add(
                        execution_cost
                            .bls12_381_additional_pair_cost
                            .saturating_mul(num_pairs.saturating_sub(1)),
                    );
                consume_compute_meter(invoke_context, cost)?;

                let g1_points = translate_slice::<PodBLSG1Point>(
                    memory_mapping,
                    g1_points_addr,
                    num_pairs,
                    invoke_context.get_check_aligned(),
                )?;

                let g2_points = translate_slice::<PodBLSG2Point>(
                    memory_mapping,
                    g2_points_addr,
                    num_pairs,
                    invoke_context.get_check_aligned(),
                )?;

                let endianness = if curve_id == BLS12_381_LE {
                    agave_bls12_381::Endianness::LE
                } else {
                    agave_bls12_381::Endianness::BE
                };

                if let Some(gt_element) = agave_bls12_381::bls12_381_pairing_map(
                    agave_bls12_381::Version::V0,
                    g1_points,
                    g2_points,
                    endianness,
                ) {
                    translate_mut!(
                        memory_mapping,
                        invoke_context.get_check_aligned(),
                        let result_ref_mut: &mut PodBLSGtElement = map(result_addr)?;
                    );
                    *result_ref_mut = gt_element;
                    Ok(SUCCESS)
                } else {
                    Ok(1)
                }
            }
            _ => {
                Err(SyscallError::InvalidAttribute.into())
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
            alt_bn128_versioned_g2_addition, alt_bn128_versioned_g2_multiplication,
            alt_bn128_versioned_pairing, Endianness, VersionedG1Addition,
            VersionedG1Multiplication, VersionedG2Addition, VersionedG2Multiplication,
            VersionedPairing, ALT_BN128_G1_POINT_SIZE, ALT_BN128_G2_POINT_SIZE,
            ALT_BN128_G1_ADD_BE, ALT_BN128_G1_MUL_BE, ALT_BN128_PAIRING_BE,
            ALT_BN128_PAIRING_ELEMENT_SIZE, ALT_BN128_PAIRING_OUTPUT_SIZE, ALT_BN128_G1_ADD_LE,
            ALT_BN128_G1_MUL_LE, ALT_BN128_PAIRING_LE, ALT_BN128_G2_ADD_BE, ALT_BN128_G2_ADD_LE,
            ALT_BN128_G2_MUL_BE, ALT_BN128_G2_MUL_LE,
        };

        // SIMD-0284: Block LE ops if the feature is not active.
        if !invoke_context.get_feature_set().alt_bn128_little_endian &&
            matches!(
                group_op,
                ALT_BN128_G1_ADD_LE
                    | ALT_BN128_G1_MUL_LE
                    | ALT_BN128_PAIRING_LE
            )
        {
            return Err(SyscallError::InvalidAttribute.into());
        }

        // SIMD-0302: Block G2 ops if the feature is not active.
        if !invoke_context.get_feature_set().enable_alt_bn128_g2_syscalls &&
            matches!(
                group_op,
                ALT_BN128_G2_ADD_BE
                    | ALT_BN128_G2_ADD_LE
                    | ALT_BN128_G2_MUL_BE
                    | ALT_BN128_G2_MUL_LE
            )
        {
            return Err(SyscallError::InvalidAttribute.into());
        }

        let execution_cost = invoke_context.get_execution_cost();
        let (cost, output): (u64, usize) = match group_op {
            ALT_BN128_G1_ADD_BE | ALT_BN128_G1_ADD_LE => (
                execution_cost.alt_bn128_g1_addition_cost,
                ALT_BN128_G1_POINT_SIZE,
            ),
            ALT_BN128_G2_ADD_BE | ALT_BN128_G2_ADD_LE => (
                execution_cost.alt_bn128_g2_addition_cost,
                ALT_BN128_G2_POINT_SIZE,
            ),
            ALT_BN128_G1_MUL_BE | ALT_BN128_G1_MUL_LE => (
                execution_cost.alt_bn128_g1_multiplication_cost,
                ALT_BN128_G1_POINT_SIZE,
            ),
            ALT_BN128_G2_MUL_BE | ALT_BN128_G2_MUL_LE => (
                execution_cost.alt_bn128_g2_multiplication_cost,
                ALT_BN128_G2_POINT_SIZE,
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
                alt_bn128_versioned_g1_addition(VersionedG1Addition::V0, input, Endianness::LE)
            }
            ALT_BN128_G2_ADD_BE => {
                alt_bn128_versioned_g2_addition(VersionedG2Addition::V0, input, Endianness::BE)
            }
            ALT_BN128_G2_ADD_LE => {
                alt_bn128_versioned_g2_addition(VersionedG2Addition::V0, input, Endianness::LE)
            }
            ALT_BN128_G1_MUL_BE => {
                alt_bn128_versioned_g1_multiplication(
                    VersionedG1Multiplication::V1,
                    input,
                    Endianness::BE
                )
            }
            ALT_BN128_G1_MUL_LE => {
                alt_bn128_versioned_g1_multiplication(
                    VersionedG1Multiplication::V1,
                    input,
                    Endianness::LE
                )
            }
            ALT_BN128_G2_MUL_BE => {
                alt_bn128_versioned_g2_multiplication(
                    VersionedG2Multiplication::V0,
                    input,
                    Endianness::BE
                )
            }
            ALT_BN128_G2_MUL_LE => {
                alt_bn128_versioned_g2_multiplication(
                    VersionedG2Multiplication::V0,
                    input,
                    Endianness::LE
                )
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
                alt_bn128_versioned_pairing(VersionedPairing::V1, input, Endianness::LE)
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
                alt_bn128_g1_compress_be, alt_bn128_g1_decompress_be,
                alt_bn128_g2_compress_be, alt_bn128_g2_decompress_be,
                alt_bn128_g1_compress_le, alt_bn128_g1_decompress_le,
                alt_bn128_g2_compress_le, alt_bn128_g2_decompress_le,
                ALT_BN128_G1_COMPRESS_BE, ALT_BN128_G1_DECOMPRESS_BE,
                ALT_BN128_G2_COMPRESS_BE, ALT_BN128_G2_DECOMPRESS_BE,
                ALT_BN128_G1_COMPRESSED_POINT_SIZE, ALT_BN128_G2_COMPRESSED_POINT_SIZE,
                ALT_BN128_G1_COMPRESS_LE, ALT_BN128_G2_COMPRESS_LE,
                ALT_BN128_G1_DECOMPRESS_LE, ALT_BN128_G2_DECOMPRESS_LE,
            }
        };

        // SIMD-0284: Block LE ops if the feature is not active.
        if !invoke_context.get_feature_set().alt_bn128_little_endian &&
            matches!(
                op,
                ALT_BN128_G1_COMPRESS_LE
                    | ALT_BN128_G2_COMPRESS_LE
                    | ALT_BN128_G1_DECOMPRESS_LE
                    | ALT_BN128_G2_DECOMPRESS_LE
            )
        {
            return Err(SyscallError::InvalidAttribute.into());
        }

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
                let Ok(result_point) = alt_bn128_g1_compress_be(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G1_COMPRESS_LE => {
                let Ok(result_point) = alt_bn128_g1_compress_le(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G1_DECOMPRESS_BE => {
                let Ok(result_point) = alt_bn128_g1_decompress_be(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G1_DECOMPRESS_LE => {
                let Ok(result_point) = alt_bn128_g1_decompress_le(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G2_COMPRESS_BE => {
                let Ok(result_point) = alt_bn128_g2_compress_be(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G2_COMPRESS_LE => {
                let Ok(result_point) = alt_bn128_g2_compress_le(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G2_DECOMPRESS_BE => {
                let Ok(result_point) = alt_bn128_g2_decompress_be(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
            }
            ALT_BN128_G2_DECOMPRESS_LE => {
                let Ok(result_point) = alt_bn128_g2_decompress_le(input) else {
                    return Ok(1);
                };
                call_result.copy_from_slice(&result_point);
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
            with_mock_invoke_context, with_mock_invoke_context_with_feature_set,
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

    macro_rules! prepare_mock_with_feature_set {
        ($invoke_context:ident,
         $program_key:ident,
         $loader_key:expr,
         $feature_set:ident $(,)?) => {
            let $program_key = Pubkey::new_unique();
            let transaction_accounts = vec![
                (
                    $loader_key,
                    AccountSharedData::new(0, 0, &native_loader::id()),
                ),
                ($program_key, AccountSharedData::new(0, 0, &$loader_key)),
            ];
            with_mock_invoke_context_with_feature_set!(
                $invoke_context,
                transaction_context,
                $feature_set,
                transaction_accounts
            );
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

        let clock_id_va = 0x300000000;
        let clock_id = Clock::id().to_bytes();

        let mut got_clock_buf_rw = vec![0; Clock::size_of()];
        let got_clock_buf_rw_va = 0x400000000;

        let got_clock_buf_ro = vec![0; Clock::size_of()];
        let got_clock_buf_ro_va = 0x500000000;

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
        invoke_context.mock_set_remaining(compute_budget.compute_unit_limit);

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

    #[test]
    fn test_syscall_bls12_381_g1_add() {
        use {
            crate::bls12_381_curve_id::{BLS12_381_G1_BE, BLS12_381_G1_LE},
            solana_curve25519::curve_syscall_traits::ADD,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let p1_bytes_be: [u8; 96] = [
            9, 86, 169, 212, 236, 245, 17, 101, 127, 183, 56, 13, 99, 100, 183, 133, 57, 107, 96,
            220, 198, 197, 2, 215, 225, 175, 212, 57, 168, 143, 104, 127, 117, 242, 180, 200, 162,
            135, 72, 155, 88, 154, 58, 90, 58, 46, 248, 176, 10, 206, 25, 112, 240, 1, 57, 89, 10,
            30, 165, 94, 164, 252, 219, 225, 133, 214, 161, 4, 118, 177, 123, 53, 57, 53, 233, 255,
            112, 117, 241, 247, 185, 195, 232, 36, 123, 31, 221, 6, 57, 176, 251, 163, 195, 39, 35,
            175,
        ];
        let p2_bytes_be: [u8; 96] = [
            13, 32, 61, 215, 83, 124, 186, 189, 82, 0, 79, 244, 67, 167, 21, 50, 48, 229, 8, 107,
            51, 15, 19, 47, 75, 77, 246, 185, 63, 66, 143, 109, 237, 211, 153, 146, 163, 175, 74,
            69, 50, 198, 235, 218, 9, 170, 225, 46, 22, 211, 116, 84, 32, 115, 130, 224, 106, 250,
            205, 143, 238, 115, 74, 207, 238, 193, 232, 16, 59, 140, 20, 252, 7, 34, 144, 47, 137,
            56, 190, 170, 235, 189, 238, 45, 97, 58, 199, 202, 45, 164, 139, 200, 190, 215, 9, 59,
        ];
        let expected_sum_be: [u8; 96] = [
            23, 62, 255, 137, 157, 188, 98, 86, 192, 102, 136, 171, 187, 49, 155, 83, 204, 133,
            217, 144, 137, 103, 15, 4, 116, 75, 127, 65, 29, 89, 223, 147, 32, 161, 91, 104, 96,
            211, 239, 102, 233, 95, 48, 130, 207, 154, 19, 189, 18, 112, 102, 145, 36, 73, 17, 27,
            47, 96, 116, 45, 56, 25, 16, 191, 56, 21, 86, 216, 133, 245, 207, 71, 158, 31, 29, 51,
            84, 185, 134, 138, 64, 68, 55, 161, 55, 153, 214, 155, 250, 21, 233, 4, 3, 117, 41,
            239,
        ];
        let p1_bytes_le: [u8; 96] = [
            176, 248, 46, 58, 90, 58, 154, 88, 155, 72, 135, 162, 200, 180, 242, 117, 127, 104,
            143, 168, 57, 212, 175, 225, 215, 2, 197, 198, 220, 96, 107, 57, 133, 183, 100, 99, 13,
            56, 183, 127, 101, 17, 245, 236, 212, 169, 86, 9, 175, 35, 39, 195, 163, 251, 176, 57,
            6, 221, 31, 123, 36, 232, 195, 185, 247, 241, 117, 112, 255, 233, 53, 57, 53, 123, 177,
            118, 4, 161, 214, 133, 225, 219, 252, 164, 94, 165, 30, 10, 89, 57, 1, 240, 112, 25,
            206, 10,
        ];
        let p2_bytes_le: [u8; 96] = [
            46, 225, 170, 9, 218, 235, 198, 50, 69, 74, 175, 163, 146, 153, 211, 237, 109, 143, 66,
            63, 185, 246, 77, 75, 47, 19, 15, 51, 107, 8, 229, 48, 50, 21, 167, 67, 244, 79, 0, 82,
            189, 186, 124, 83, 215, 61, 32, 13, 59, 9, 215, 190, 200, 139, 164, 45, 202, 199, 58,
            97, 45, 238, 189, 235, 170, 190, 56, 137, 47, 144, 34, 7, 252, 20, 140, 59, 16, 232,
            193, 238, 207, 74, 115, 238, 143, 205, 250, 106, 224, 130, 115, 32, 84, 116, 211, 22,
        ];
        let expected_sum_le: [u8; 96] = [
            189, 19, 154, 207, 130, 48, 95, 233, 102, 239, 211, 96, 104, 91, 161, 32, 147, 223, 89,
            29, 65, 127, 75, 116, 4, 15, 103, 137, 144, 217, 133, 204, 83, 155, 49, 187, 171, 136,
            102, 192, 86, 98, 188, 157, 137, 255, 62, 23, 239, 41, 117, 3, 4, 233, 21, 250, 155,
            214, 153, 55, 161, 55, 68, 64, 138, 134, 185, 84, 51, 29, 31, 158, 71, 207, 245, 133,
            216, 86, 21, 56, 191, 16, 25, 56, 45, 116, 96, 47, 27, 17, 73, 36, 145, 102, 112, 18,
        ];

        let p1_be_va = 0x100000000;
        let p2_be_va = 0x200000000;
        let p1_le_va = 0x300000000;
        let p2_le_va = 0x400000000;
        let result_be_va = 0x500000000;
        let result_le_va = 0x600000000;

        let mut result_be_buf = [0u8; 96];
        let mut result_le_buf = [0u8; 96];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&p1_bytes_be, p1_be_va),
                MemoryRegion::new_readonly(&p2_bytes_be, p2_be_va),
                MemoryRegion::new_writable(&mut result_be_buf, result_be_va),
                MemoryRegion::new_readonly(&p1_bytes_le, p1_le_va),
                MemoryRegion::new_readonly(&p2_bytes_le, p2_le_va),
                MemoryRegion::new_writable(&mut result_le_buf, result_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g1_add_cost = invoke_context.get_execution_cost().bls12_381_g1_add_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g1_add_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            ADD,
            p1_be_va,
            p2_be_va,
            result_be_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_be_buf, expected_sum_be);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G1_LE,
            ADD,
            p1_le_va,
            p2_le_va,
            result_le_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_le_buf, expected_sum_le);
    }

    #[test]
    fn test_syscall_bls12_381_g1_sub() {
        use {
            crate::bls12_381_curve_id::{BLS12_381_G1_BE, BLS12_381_G1_LE},
            solana_curve25519::curve_syscall_traits::SUB,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let sub_p1_be: [u8; 96] = [
            6, 126, 67, 177, 221, 168, 219, 147, 17, 32, 109, 112, 204, 95, 207, 179, 227, 202, 32,
            250, 118, 43, 195, 105, 176, 47, 188, 43, 181, 226, 123, 119, 132, 240, 97, 172, 225,
            247, 180, 76, 58, 229, 188, 121, 247, 28, 245, 198, 17, 128, 94, 239, 206, 10, 10, 20,
            148, 186, 226, 202, 12, 196, 71, 72, 167, 44, 87, 64, 24, 214, 238, 218, 6, 166, 113,
            165, 178, 8, 221, 0, 21, 154, 72, 160, 158, 70, 46, 244, 127, 4, 250, 158, 31, 2, 130,
            152,
        ];
        let sub_p2_be: [u8; 96] = [
            12, 173, 131, 106, 17, 172, 169, 46, 205, 228, 83, 25, 204, 216, 118, 223, 16, 102, 52,
            235, 202, 255, 183, 91, 99, 78, 141, 169, 14, 244, 161, 28, 240, 32, 214, 46, 0, 93,
            106, 73, 41, 176, 220, 160, 251, 37, 18, 110, 15, 86, 67, 210, 137, 114, 71, 220, 167,
            121, 177, 224, 142, 151, 152, 29, 206, 12, 35, 6, 46, 60, 53, 127, 84, 78, 231, 88, 49,
            95, 219, 36, 224, 182, 0, 253, 136, 115, 59, 15, 80, 229, 136, 103, 27, 211, 120, 90,
        ];
        let expected_sub_be: [u8; 96] = [
            13, 144, 131, 116, 67, 229, 136, 165, 135, 146, 181, 191, 197, 215, 68, 126, 103, 158,
            231, 50, 49, 105, 8, 243, 53, 209, 99, 16, 39, 177, 211, 99, 128, 164, 37, 101, 139,
            186, 14, 225, 84, 210, 120, 16, 203, 115, 160, 49, 10, 243, 68, 241, 87, 193, 186, 179,
            87, 214, 88, 39, 123, 126, 136, 31, 178, 134, 203, 222, 127, 206, 218, 240, 135, 183,
            93, 145, 136, 148, 174, 238, 159, 0, 117, 212, 171, 247, 148, 197, 206, 7, 225, 81,
            114, 74, 63, 201,
        ];
        let sub_p1_le: [u8; 96] = [
            198, 245, 28, 247, 121, 188, 229, 58, 76, 180, 247, 225, 172, 97, 240, 132, 119, 123,
            226, 181, 43, 188, 47, 176, 105, 195, 43, 118, 250, 32, 202, 227, 179, 207, 95, 204,
            112, 109, 32, 17, 147, 219, 168, 221, 177, 67, 126, 6, 152, 130, 2, 31, 158, 250, 4,
            127, 244, 46, 70, 158, 160, 72, 154, 21, 0, 221, 8, 178, 165, 113, 166, 6, 218, 238,
            214, 24, 64, 87, 44, 167, 72, 71, 196, 12, 202, 226, 186, 148, 20, 10, 10, 206, 239,
            94, 128, 17,
        ];
        let sub_p2_le: [u8; 96] = [
            110, 18, 37, 251, 160, 220, 176, 41, 73, 106, 93, 0, 46, 214, 32, 240, 28, 161, 244,
            14, 169, 141, 78, 99, 91, 183, 255, 202, 235, 52, 102, 16, 223, 118, 216, 204, 25, 83,
            228, 205, 46, 169, 172, 17, 106, 131, 173, 12, 90, 120, 211, 27, 103, 136, 229, 80, 15,
            59, 115, 136, 253, 0, 182, 224, 36, 219, 95, 49, 88, 231, 78, 84, 127, 53, 60, 46, 6,
            35, 12, 206, 29, 152, 151, 142, 224, 177, 121, 167, 220, 71, 114, 137, 210, 67, 86, 15,
        ];
        let expected_sub_le: [u8; 96] = [
            49, 160, 115, 203, 16, 120, 210, 84, 225, 14, 186, 139, 101, 37, 164, 128, 99, 211,
            177, 39, 16, 99, 209, 53, 243, 8, 105, 49, 50, 231, 158, 103, 126, 68, 215, 197, 191,
            181, 146, 135, 165, 136, 229, 67, 116, 131, 144, 13, 201, 63, 74, 114, 81, 225, 7, 206,
            197, 148, 247, 171, 212, 117, 0, 159, 238, 174, 148, 136, 145, 93, 183, 135, 240, 218,
            206, 127, 222, 203, 134, 178, 31, 136, 126, 123, 39, 88, 214, 87, 179, 186, 193, 87,
            241, 68, 243, 10,
        ];

        let p1_be_va = 0x100000000;
        let p2_be_va = 0x200000000;
        let p1_le_va = 0x300000000;
        let p2_le_va = 0x400000000;
        let result_be_va = 0x500000000;
        let result_le_va = 0x600000000;

        let mut result_be_buf = [0u8; 96];
        let mut result_le_buf = [0u8; 96];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&sub_p1_be, p1_be_va),
                MemoryRegion::new_readonly(&sub_p2_be, p2_be_va),
                MemoryRegion::new_writable(&mut result_be_buf, result_be_va),
                MemoryRegion::new_readonly(&sub_p1_le, p1_le_va),
                MemoryRegion::new_readonly(&sub_p2_le, p2_le_va),
                MemoryRegion::new_writable(&mut result_le_buf, result_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g1_subtract_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g1_subtract_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g1_subtract_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            SUB,
            p1_be_va,
            p2_be_va,
            result_be_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_be_buf, expected_sub_be);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G1_LE,
            SUB,
            p1_le_va,
            p2_le_va,
            result_le_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_le_buf, expected_sub_le);
    }

    #[test]
    fn test_syscall_bls12_381_g1_mul() {
        use {
            crate::bls12_381_curve_id::{BLS12_381_G1_BE, BLS12_381_G1_LE},
            solana_curve25519::curve_syscall_traits::MUL,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let mul_point_be: [u8; 96] = [
            20, 18, 233, 201, 110, 206, 56, 32, 8, 44, 140, 121, 37, 196, 157, 56, 180, 134, 164,
            33, 180, 130, 147, 7, 26, 239, 183, 163, 219, 85, 143, 197, 247, 243, 117, 252, 201,
            171, 156, 90, 210, 7, 43, 92, 89, 130, 165, 224, 5, 101, 24, 54, 189, 22, 73, 76, 145,
            136, 99, 59, 51, 255, 124, 43, 61, 8, 121, 30, 118, 90, 254, 12, 126, 92, 152, 78, 44,
            231, 126, 56, 220, 35, 54, 117, 2, 175, 190, 105, 138, 188, 202, 36, 171, 12, 231, 225,
        ];
        let mul_scalar_be: [u8; 32] = [
            29, 192, 111, 151, 187, 37, 109, 91, 129, 223, 188, 225, 117, 3, 120, 162, 107, 66,
            159, 255, 61, 128, 41, 32, 242, 95, 232, 202, 106, 188, 154, 147,
        ];
        let expected_mul_be: [u8; 96] = [
            22, 101, 72, 255, 3, 247, 39, 218, 234, 117, 208, 91, 158, 114, 126, 55, 166, 71, 227,
            205, 6, 124, 55, 255, 167, 66, 154, 237, 83, 143, 8, 179, 98, 185, 162, 164, 170, 62,
            141, 4, 1, 179, 41, 49, 95, 212, 139, 227, 18, 125, 245, 10, 169, 201, 171, 172, 152,
            1, 105, 81, 159, 160, 252, 184, 80, 59, 165, 170, 185, 114, 248, 208, 228, 111, 229,
            200, 221, 204, 9, 120, 153, 142, 88, 240, 228, 164, 157, 79, 72, 55, 119, 239, 56, 104,
            54, 58,
        ];
        let mul_point_le: [u8; 96] = [
            224, 165, 130, 89, 92, 43, 7, 210, 90, 156, 171, 201, 252, 117, 243, 247, 197, 143, 85,
            219, 163, 183, 239, 26, 7, 147, 130, 180, 33, 164, 134, 180, 56, 157, 196, 37, 121,
            140, 44, 8, 32, 56, 206, 110, 201, 233, 18, 20, 225, 231, 12, 171, 36, 202, 188, 138,
            105, 190, 175, 2, 117, 54, 35, 220, 56, 126, 231, 44, 78, 152, 92, 126, 12, 254, 90,
            118, 30, 121, 8, 61, 43, 124, 255, 51, 59, 99, 136, 145, 76, 73, 22, 189, 54, 24, 101,
            5,
        ];
        let mul_scalar_le: [u8; 32] = [
            147, 154, 188, 106, 202, 232, 95, 242, 32, 41, 128, 61, 255, 159, 66, 107, 162, 120, 3,
            117, 225, 188, 223, 129, 91, 109, 37, 187, 151, 111, 192, 29,
        ];
        let expected_mul_le: [u8; 96] = [
            227, 139, 212, 95, 49, 41, 179, 1, 4, 141, 62, 170, 164, 162, 185, 98, 179, 8, 143, 83,
            237, 154, 66, 167, 255, 55, 124, 6, 205, 227, 71, 166, 55, 126, 114, 158, 91, 208, 117,
            234, 218, 39, 247, 3, 255, 72, 101, 22, 58, 54, 104, 56, 239, 119, 55, 72, 79, 157,
            164, 228, 240, 88, 142, 153, 120, 9, 204, 221, 200, 229, 111, 228, 208, 248, 114, 185,
            170, 165, 59, 80, 184, 252, 160, 159, 81, 105, 1, 152, 172, 171, 201, 169, 10, 245,
            125, 18,
        ];

        let scalar_be_va = 0x100000000;
        let point_be_va = 0x200000000;
        let scalar_le_va = 0x300000000;
        let point_le_va = 0x400000000;
        let result_be_va = 0x500000000;
        let result_le_va = 0x600000000;

        let mut result_be_buf = [0u8; 96];
        let mut result_le_buf = [0u8; 96];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mul_scalar_be, scalar_be_va),
                MemoryRegion::new_readonly(&mul_point_be, point_be_va),
                MemoryRegion::new_writable(&mut result_be_buf, result_be_va),
                MemoryRegion::new_readonly(&mul_scalar_le, scalar_le_va),
                MemoryRegion::new_readonly(&mul_point_le, point_le_va),
                MemoryRegion::new_writable(&mut result_le_buf, result_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g1_multiply_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g1_multiply_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g1_multiply_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            MUL,
            scalar_be_va,
            point_be_va,
            result_be_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_be_buf, expected_mul_be);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G1_LE,
            MUL,
            scalar_le_va,
            point_le_va,
            result_le_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_le_buf, expected_mul_le);
    }

    #[test]
    fn test_syscall_bls12_381_g2_add() {
        use {
            crate::bls12_381_curve_id::{BLS12_381_G2_BE, BLS12_381_G2_LE},
            solana_curve25519::curve_syscall_traits::ADD,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let p1_bytes_be: [u8; 192] = [
            11, 83, 21, 62, 4, 174, 123, 131, 163, 19, 62, 216, 192, 48, 25, 184, 57, 207, 80, 70,
            253, 51, 129, 169, 87, 182, 142, 1, 148, 102, 203, 99, 86, 111, 207, 55, 204, 117, 82,
            138, 199, 89, 131, 207, 158, 244, 204, 139, 18, 151, 214, 201, 158, 39, 101, 252, 189,
            53, 251, 236, 205, 27, 152, 163, 232, 101, 53, 197, 18, 238, 241, 70, 182, 113, 111,
            249, 99, 122, 42, 220, 55, 127, 55, 247, 172, 164, 183, 169, 146, 229, 218, 185, 144,
            176, 86, 174, 21, 132, 150, 29, 241, 241, 215, 77, 12, 75, 238, 103, 23, 90, 189, 191,
            85, 72, 181, 214, 85, 253, 183, 150, 158, 8, 250, 178, 220, 169, 215, 243, 146, 213,
            150, 12, 6, 40, 188, 197, 56, 210, 46, 125, 87, 5, 17, 7, 24, 27, 160, 22, 99, 114, 9,
            7, 244, 108, 179, 201, 38, 33, 153, 219, 10, 211, 2, 212, 74, 95, 151, 223, 200, 96,
            121, 166, 10, 186, 122, 40, 222, 87, 34, 227, 49, 166, 195, 139, 37, 221, 44, 227, 86,
            119, 190, 41,
        ];
        let p2_bytes_be: [u8; 192] = [
            14, 110, 180, 174, 46, 74, 145, 125, 94, 28, 39, 205, 107, 126, 53, 188, 36, 69, 162,
            98, 105, 79, 49, 148, 136, 229, 5, 128, 197, 187, 0, 234, 141, 201, 246, 223, 103, 75,
            177, 33, 2, 75, 90, 33, 139, 152, 156, 89, 25, 91, 158, 100, 20, 12, 135, 130, 191,
            181, 5, 41, 94, 195, 89, 36, 181, 111, 238, 24, 187, 178, 179, 143, 17, 181, 68, 203,
            184, 134, 185, 195, 176, 27, 90, 2, 29, 165, 209, 16, 143, 11, 224, 251, 63, 188, 218,
            41, 23, 71, 91, 90, 202, 108, 80, 160, 200, 194, 162, 109, 200, 96, 5, 102, 156, 245,
            43, 247, 221, 139, 148, 254, 253, 183, 161, 83, 253, 247, 22, 71, 133, 93, 36, 127,
            162, 248, 49, 64, 173, 201, 17, 210, 8, 214, 18, 65, 7, 222, 11, 4, 120, 17, 85, 49,
            205, 95, 132, 208, 152, 136, 92, 19, 195, 176, 136, 39, 90, 207, 17, 195, 14, 215, 33,
            191, 232, 59, 3, 86, 78, 78, 149, 165, 179, 145, 161, 190, 247, 67, 243, 252, 137, 1,
            39, 71,
        ];
        let expected_sum_be: [u8; 192] = [
            21, 157, 10, 251, 156, 56, 24, 174, 24, 91, 98, 201, 33, 37, 68, 76, 41, 161, 12, 166,
            16, 128, 161, 31, 108, 31, 92, 216, 56, 197, 198, 66, 210, 6, 64, 106, 154, 96, 135,
            57, 170, 119, 220, 210, 238, 73, 98, 83, 15, 146, 74, 122, 70, 40, 186, 123, 191, 139,
            11, 249, 221, 20, 12, 62, 81, 37, 191, 22, 248, 113, 78, 124, 29, 157, 228, 220, 187,
            6, 252, 15, 59, 236, 98, 198, 252, 205, 176, 190, 192, 199, 154, 213, 92, 126, 189, 55,
            2, 109, 8, 15, 128, 190, 31, 106, 180, 130, 96, 215, 125, 50, 11, 124, 71, 119, 83, 28,
            65, 209, 128, 47, 7, 46, 212, 157, 230, 199, 51, 98, 143, 220, 157, 254, 179, 203, 186,
            116, 41, 76, 35, 28, 123, 207, 54, 17, 5, 248, 36, 247, 193, 201, 116, 118, 202, 201,
            125, 201, 200, 13, 68, 244, 39, 207, 70, 206, 12, 117, 206, 192, 9, 232, 62, 33, 137,
            88, 73, 16, 121, 190, 139, 91, 158, 80, 147, 207, 125, 23, 177, 93, 227, 132, 103, 89,
        ];
        let p1_bytes_le: [u8; 192] = [
            174, 86, 176, 144, 185, 218, 229, 146, 169, 183, 164, 172, 247, 55, 127, 55, 220, 42,
            122, 99, 249, 111, 113, 182, 70, 241, 238, 18, 197, 53, 101, 232, 163, 152, 27, 205,
            236, 251, 53, 189, 252, 101, 39, 158, 201, 214, 151, 18, 139, 204, 244, 158, 207, 131,
            89, 199, 138, 82, 117, 204, 55, 207, 111, 86, 99, 203, 102, 148, 1, 142, 182, 87, 169,
            129, 51, 253, 70, 80, 207, 57, 184, 25, 48, 192, 216, 62, 19, 163, 131, 123, 174, 4,
            62, 21, 83, 11, 41, 190, 119, 86, 227, 44, 221, 37, 139, 195, 166, 49, 227, 34, 87,
            222, 40, 122, 186, 10, 166, 121, 96, 200, 223, 151, 95, 74, 212, 2, 211, 10, 219, 153,
            33, 38, 201, 179, 108, 244, 7, 9, 114, 99, 22, 160, 27, 24, 7, 17, 5, 87, 125, 46, 210,
            56, 197, 188, 40, 6, 12, 150, 213, 146, 243, 215, 169, 220, 178, 250, 8, 158, 150, 183,
            253, 85, 214, 181, 72, 85, 191, 189, 90, 23, 103, 238, 75, 12, 77, 215, 241, 241, 29,
            150, 132, 21,
        ];
        let p2_bytes_le: [u8; 192] = [
            41, 218, 188, 63, 251, 224, 11, 143, 16, 209, 165, 29, 2, 90, 27, 176, 195, 185, 134,
            184, 203, 68, 181, 17, 143, 179, 178, 187, 24, 238, 111, 181, 36, 89, 195, 94, 41, 5,
            181, 191, 130, 135, 12, 20, 100, 158, 91, 25, 89, 156, 152, 139, 33, 90, 75, 2, 33,
            177, 75, 103, 223, 246, 201, 141, 234, 0, 187, 197, 128, 5, 229, 136, 148, 49, 79, 105,
            98, 162, 69, 36, 188, 53, 126, 107, 205, 39, 28, 94, 125, 145, 74, 46, 174, 180, 110,
            14, 71, 39, 1, 137, 252, 243, 67, 247, 190, 161, 145, 179, 165, 149, 78, 78, 86, 3, 59,
            232, 191, 33, 215, 14, 195, 17, 207, 90, 39, 136, 176, 195, 19, 92, 136, 152, 208, 132,
            95, 205, 49, 85, 17, 120, 4, 11, 222, 7, 65, 18, 214, 8, 210, 17, 201, 173, 64, 49,
            248, 162, 127, 36, 93, 133, 71, 22, 247, 253, 83, 161, 183, 253, 254, 148, 139, 221,
            247, 43, 245, 156, 102, 5, 96, 200, 109, 162, 194, 200, 160, 80, 108, 202, 90, 91, 71,
            23,
        ];
        let expected_sum_le: [u8; 192] = [
            55, 189, 126, 92, 213, 154, 199, 192, 190, 176, 205, 252, 198, 98, 236, 59, 15, 252, 6,
            187, 220, 228, 157, 29, 124, 78, 113, 248, 22, 191, 37, 81, 62, 12, 20, 221, 249, 11,
            139, 191, 123, 186, 40, 70, 122, 74, 146, 15, 83, 98, 73, 238, 210, 220, 119, 170, 57,
            135, 96, 154, 106, 64, 6, 210, 66, 198, 197, 56, 216, 92, 31, 108, 31, 161, 128, 16,
            166, 12, 161, 41, 76, 68, 37, 33, 201, 98, 91, 24, 174, 24, 56, 156, 251, 10, 157, 21,
            89, 103, 132, 227, 93, 177, 23, 125, 207, 147, 80, 158, 91, 139, 190, 121, 16, 73, 88,
            137, 33, 62, 232, 9, 192, 206, 117, 12, 206, 70, 207, 39, 244, 68, 13, 200, 201, 125,
            201, 202, 118, 116, 201, 193, 247, 36, 248, 5, 17, 54, 207, 123, 28, 35, 76, 41, 116,
            186, 203, 179, 254, 157, 220, 143, 98, 51, 199, 230, 157, 212, 46, 7, 47, 128, 209, 65,
            28, 83, 119, 71, 124, 11, 50, 125, 215, 96, 130, 180, 106, 31, 190, 128, 15, 8, 109, 2,
        ];

        let p1_be_va = 0x100000000;
        let p2_be_va = 0x200000000;
        let p1_le_va = 0x300000000;
        let p2_le_va = 0x400000000;
        let result_be_va = 0x500000000;
        let result_le_va = 0x600000000;

        let mut result_be_buf = [0u8; 192];
        let mut result_le_buf = [0u8; 192];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&p1_bytes_be, p1_be_va),
                MemoryRegion::new_readonly(&p2_bytes_be, p2_be_va),
                MemoryRegion::new_writable(&mut result_be_buf, result_be_va),
                MemoryRegion::new_readonly(&p1_bytes_le, p1_le_va),
                MemoryRegion::new_readonly(&p2_bytes_le, p2_le_va),
                MemoryRegion::new_writable(&mut result_le_buf, result_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_add_cost = invoke_context.get_execution_cost().bls12_381_g2_add_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g2_add_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G2_BE,
            ADD,
            p1_be_va,
            p2_be_va,
            result_be_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_be_buf, expected_sum_be);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G2_LE,
            ADD,
            p1_le_va,
            p2_le_va,
            result_le_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_le_buf, expected_sum_le);
    }

    #[test]
    fn test_syscall_bls12_381_g2_sub() {
        use {
            crate::bls12_381_curve_id::{BLS12_381_G2_BE, BLS12_381_G2_LE},
            solana_curve25519::curve_syscall_traits::SUB,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let sub_p1_be: [u8; 192] = [
            1, 111, 113, 42, 165, 128, 194, 26, 130, 142, 58, 198, 61, 244, 113, 64, 25, 96, 196,
            12, 211, 55, 213, 85, 109, 210, 211, 177, 96, 48, 15, 122, 155, 173, 166, 16, 113, 95,
            253, 69, 196, 15, 187, 201, 207, 255, 81, 176, 15, 77, 24, 199, 78, 142, 23, 177, 55,
            118, 62, 248, 123, 41, 213, 72, 169, 177, 5, 176, 197, 158, 62, 1, 5, 219, 190, 92, 36,
            37, 117, 162, 202, 9, 231, 199, 13, 72, 102, 36, 246, 241, 52, 68, 185, 44, 238, 23,
            23, 1, 192, 28, 61, 103, 236, 74, 46, 28, 64, 67, 194, 243, 208, 186, 46, 201, 142, 7,
            166, 139, 114, 215, 101, 234, 108, 184, 93, 135, 61, 176, 154, 208, 28, 79, 210, 132,
            96, 21, 199, 11, 73, 210, 40, 241, 107, 215, 8, 203, 156, 2, 211, 33, 203, 196, 124,
            172, 148, 232, 121, 116, 109, 226, 15, 13, 147, 241, 20, 70, 28, 10, 17, 51, 143, 140,
            35, 127, 109, 7, 202, 220, 208, 97, 11, 167, 119, 94, 192, 92, 165, 215, 230, 160, 16,
            56,
        ];
        let sub_p2_be: [u8; 192] = [
            14, 73, 101, 89, 211, 85, 5, 115, 148, 81, 82, 216, 141, 148, 50, 174, 17, 86, 246,
            146, 42, 230, 181, 250, 40, 64, 248, 121, 6, 167, 117, 190, 219, 96, 57, 80, 127, 234,
            141, 179, 154, 109, 5, 82, 233, 254, 7, 48, 5, 108, 253, 196, 16, 144, 81, 140, 252,
            184, 236, 193, 97, 200, 129, 223, 132, 28, 135, 121, 129, 129, 60, 33, 77, 43, 181,
            180, 60, 224, 108, 127, 207, 112, 54, 66, 81, 185, 166, 120, 54, 169, 55, 238, 32, 219,
            172, 212, 24, 165, 106, 207, 20, 68, 130, 233, 190, 75, 177, 17, 157, 112, 174, 88,
            189, 182, 126, 219, 114, 136, 67, 15, 167, 133, 50, 172, 124, 94, 8, 149, 203, 232, 35,
            218, 144, 142, 74, 150, 94, 182, 33, 106, 111, 120, 203, 59, 10, 121, 79, 248, 118,
            165, 232, 57, 87, 60, 42, 223, 98, 104, 158, 238, 68, 152, 59, 19, 172, 89, 20, 238,
            63, 49, 204, 138, 108, 195, 10, 233, 81, 79, 215, 107, 43, 197, 190, 231, 15, 14, 251,
            203, 179, 205, 224, 195,
        ];
        let expected_sub_be: [u8; 192] = [
            15, 192, 220, 234, 246, 126, 141, 163, 107, 162, 43, 117, 171, 158, 195, 132, 196, 214,
            237, 133, 98, 133, 112, 248, 161, 148, 3, 163, 20, 26, 49, 136, 161, 244, 36, 179, 237,
            204, 58, 22, 51, 106, 0, 4, 239, 244, 242, 89, 5, 14, 149, 31, 78, 213, 70, 153, 147,
            43, 84, 19, 223, 100, 235, 61, 172, 66, 136, 201, 11, 81, 168, 136, 207, 46, 198, 208,
            171, 144, 187, 35, 77, 58, 186, 147, 191, 243, 9, 12, 224, 22, 230, 36, 112, 246, 114,
            19, 13, 116, 186, 62, 158, 176, 201, 150, 187, 13, 32, 135, 140, 108, 178, 174, 90,
            212, 50, 184, 238, 17, 229, 167, 195, 104, 179, 156, 166, 251, 99, 115, 133, 25, 144,
            101, 45, 70, 19, 86, 91, 247, 236, 93, 252, 14, 106, 212, 15, 42, 62, 104, 162, 216, 8,
            180, 156, 52, 254, 179, 29, 95, 94, 16, 245, 215, 165, 67, 115, 50, 186, 190, 227, 213,
            71, 126, 29, 81, 217, 43, 157, 12, 100, 105, 211, 172, 101, 212, 73, 140, 149, 109,
            252, 180, 98, 22,
        ];
        let sub_p1_le: [u8; 192] = [
            23, 238, 44, 185, 68, 52, 241, 246, 36, 102, 72, 13, 199, 231, 9, 202, 162, 117, 37,
            36, 92, 190, 219, 5, 1, 62, 158, 197, 176, 5, 177, 169, 72, 213, 41, 123, 248, 62, 118,
            55, 177, 23, 142, 78, 199, 24, 77, 15, 176, 81, 255, 207, 201, 187, 15, 196, 69, 253,
            95, 113, 16, 166, 173, 155, 122, 15, 48, 96, 177, 211, 210, 109, 85, 213, 55, 211, 12,
            196, 96, 25, 64, 113, 244, 61, 198, 58, 142, 130, 26, 194, 128, 165, 42, 113, 111, 1,
            56, 16, 160, 230, 215, 165, 92, 192, 94, 119, 167, 11, 97, 208, 220, 202, 7, 109, 127,
            35, 140, 143, 51, 17, 10, 28, 70, 20, 241, 147, 13, 15, 226, 109, 116, 121, 232, 148,
            172, 124, 196, 203, 33, 211, 2, 156, 203, 8, 215, 107, 241, 40, 210, 73, 11, 199, 21,
            96, 132, 210, 79, 28, 208, 154, 176, 61, 135, 93, 184, 108, 234, 101, 215, 114, 139,
            166, 7, 142, 201, 46, 186, 208, 243, 194, 67, 64, 28, 46, 74, 236, 103, 61, 28, 192, 1,
            23,
        ];
        let sub_p2_le: [u8; 192] = [
            212, 172, 219, 32, 238, 55, 169, 54, 120, 166, 185, 81, 66, 54, 112, 207, 127, 108,
            224, 60, 180, 181, 43, 77, 33, 60, 129, 129, 121, 135, 28, 132, 223, 129, 200, 97, 193,
            236, 184, 252, 140, 81, 144, 16, 196, 253, 108, 5, 48, 7, 254, 233, 82, 5, 109, 154,
            179, 141, 234, 127, 80, 57, 96, 219, 190, 117, 167, 6, 121, 248, 64, 40, 250, 181, 230,
            42, 146, 246, 86, 17, 174, 50, 148, 141, 216, 82, 81, 148, 115, 5, 85, 211, 89, 101,
            73, 14, 195, 224, 205, 179, 203, 251, 14, 15, 231, 190, 197, 43, 107, 215, 79, 81, 233,
            10, 195, 108, 138, 204, 49, 63, 238, 20, 89, 172, 19, 59, 152, 68, 238, 158, 104, 98,
            223, 42, 60, 87, 57, 232, 165, 118, 248, 79, 121, 10, 59, 203, 120, 111, 106, 33, 182,
            94, 150, 74, 142, 144, 218, 35, 232, 203, 149, 8, 94, 124, 172, 50, 133, 167, 15, 67,
            136, 114, 219, 126, 182, 189, 88, 174, 112, 157, 17, 177, 75, 190, 233, 130, 68, 20,
            207, 106, 165, 24,
        ];
        let expected_sub_le: [u8; 192] = [
            19, 114, 246, 112, 36, 230, 22, 224, 12, 9, 243, 191, 147, 186, 58, 77, 35, 187, 144,
            171, 208, 198, 46, 207, 136, 168, 81, 11, 201, 136, 66, 172, 61, 235, 100, 223, 19, 84,
            43, 147, 153, 70, 213, 78, 31, 149, 14, 5, 89, 242, 244, 239, 4, 0, 106, 51, 22, 58,
            204, 237, 179, 36, 244, 161, 136, 49, 26, 20, 163, 3, 148, 161, 248, 112, 133, 98, 133,
            237, 214, 196, 132, 195, 158, 171, 117, 43, 162, 107, 163, 141, 126, 246, 234, 220,
            192, 15, 22, 98, 180, 252, 109, 149, 140, 73, 212, 101, 172, 211, 105, 100, 12, 157,
            43, 217, 81, 29, 126, 71, 213, 227, 190, 186, 50, 115, 67, 165, 215, 245, 16, 94, 95,
            29, 179, 254, 52, 156, 180, 8, 216, 162, 104, 62, 42, 15, 212, 106, 14, 252, 93, 236,
            247, 91, 86, 19, 70, 45, 101, 144, 25, 133, 115, 99, 251, 166, 156, 179, 104, 195, 167,
            229, 17, 238, 184, 50, 212, 90, 174, 178, 108, 140, 135, 32, 13, 187, 150, 201, 176,
            158, 62, 186, 116, 13,
        ];

        let p1_be_va = 0x100000000;
        let p2_be_va = 0x200000000;
        let p1_le_va = 0x300000000;
        let p2_le_va = 0x400000000;
        let result_be_va = 0x500000000;
        let result_le_va = 0x600000000;

        let mut result_be_buf = [0u8; 192];
        let mut result_le_buf = [0u8; 192];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&sub_p1_be, p1_be_va),
                MemoryRegion::new_readonly(&sub_p2_be, p2_be_va),
                MemoryRegion::new_writable(&mut result_be_buf, result_be_va),
                MemoryRegion::new_readonly(&sub_p1_le, p1_le_va),
                MemoryRegion::new_readonly(&sub_p2_le, p2_le_va),
                MemoryRegion::new_writable(&mut result_le_buf, result_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_subtract_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g2_subtract_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g2_subtract_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G2_BE,
            SUB,
            p1_be_va,
            p2_be_va,
            result_be_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_be_buf, expected_sub_be);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G2_LE,
            SUB,
            p1_le_va,
            p2_le_va,
            result_le_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_le_buf, expected_sub_le);
    }

    #[test]
    fn test_syscall_bls12_381_g2_mul() {
        use {
            crate::bls12_381_curve_id::{BLS12_381_G2_BE, BLS12_381_G2_LE},
            solana_curve25519::curve_syscall_traits::MUL,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let mul_point_be: [u8; 192] = [
            1, 95, 16, 90, 117, 185, 253, 76, 25, 68, 54, 111, 154, 161, 125, 203, 121, 4, 154, 67,
            205, 157, 76, 9, 128, 224, 37, 81, 214, 226, 71, 59, 224, 187, 152, 153, 199, 62, 58,
            74, 137, 245, 46, 101, 155, 17, 212, 64, 5, 134, 0, 185, 19, 132, 205, 101, 77, 204,
            118, 63, 71, 172, 208, 29, 210, 61, 51, 4, 190, 191, 211, 175, 105, 245, 204, 57, 56,
            84, 210, 184, 235, 169, 231, 161, 128, 83, 252, 234, 227, 255, 166, 219, 201, 176, 169,
            16, 20, 218, 203, 38, 181, 98, 213, 89, 152, 123, 230, 201, 4, 95, 42, 86, 29, 137, 67,
            233, 230, 161, 206, 231, 201, 176, 79, 12, 197, 56, 212, 36, 235, 216, 160, 27, 221,
            99, 124, 220, 133, 76, 123, 209, 200, 78, 122, 36, 16, 171, 18, 247, 111, 111, 132, 38,
            240, 183, 27, 76, 135, 211, 136, 202, 55, 93, 246, 235, 191, 146, 183, 161, 110, 129,
            4, 58, 238, 59, 77, 242, 56, 88, 96, 150, 146, 247, 137, 230, 137, 35, 9, 108, 95, 127,
            75, 78,
        ];
        let mul_scalar_be: [u8; 32] = [
            29, 192, 111, 151, 187, 37, 109, 91, 129, 223, 188, 225, 117, 3, 120, 162, 107, 66,
            159, 255, 61, 128, 41, 32, 242, 95, 232, 202, 106, 188, 154, 147,
        ];
        let expected_mul_be: [u8; 192] = [
            10, 92, 88, 192, 26, 200, 38, 128, 188, 148, 254, 16, 202, 39, 174, 252, 33, 111, 41,
            121, 211, 9, 209, 138, 43, 104, 122, 214, 4, 251, 34, 81, 36, 92, 143, 19, 151, 213,
            111, 240, 100, 15, 33, 74, 123, 143, 181, 153, 6, 107, 82, 96, 141, 147, 63, 200, 13,
            31, 66, 5, 184, 135, 24, 82, 189, 240, 58, 250, 48, 61, 132, 13, 23, 240, 31, 238, 252,
            33, 191, 241, 38, 90, 221, 201, 164, 137, 98, 92, 148, 246, 225, 22, 239, 99, 97, 179,
            20, 251, 39, 114, 14, 156, 165, 182, 58, 233, 100, 41, 34, 59, 119, 103, 40, 206, 50,
            175, 223, 126, 146, 17, 161, 14, 84, 43, 149, 58, 212, 197, 250, 15, 208, 122, 33, 4,
            87, 219, 82, 201, 12, 11, 44, 76, 59, 182, 18, 76, 38, 184, 175, 11, 211, 4, 64, 133,
            41, 104, 185, 153, 63, 246, 39, 145, 38, 113, 162, 183, 77, 2, 51, 134, 243, 196, 74,
            111, 183, 169, 222, 228, 191, 53, 129, 53, 186, 94, 97, 144, 31, 117, 218, 207, 214,
            189,
        ];
        let mul_point_le: [u8; 192] = [
            16, 169, 176, 201, 219, 166, 255, 227, 234, 252, 83, 128, 161, 231, 169, 235, 184, 210,
            84, 56, 57, 204, 245, 105, 175, 211, 191, 190, 4, 51, 61, 210, 29, 208, 172, 71, 63,
            118, 204, 77, 101, 205, 132, 19, 185, 0, 134, 5, 64, 212, 17, 155, 101, 46, 245, 137,
            74, 58, 62, 199, 153, 152, 187, 224, 59, 71, 226, 214, 81, 37, 224, 128, 9, 76, 157,
            205, 67, 154, 4, 121, 203, 125, 161, 154, 111, 54, 68, 25, 76, 253, 185, 117, 90, 16,
            95, 1, 78, 75, 127, 95, 108, 9, 35, 137, 230, 137, 247, 146, 150, 96, 88, 56, 242, 77,
            59, 238, 58, 4, 129, 110, 161, 183, 146, 191, 235, 246, 93, 55, 202, 136, 211, 135, 76,
            27, 183, 240, 38, 132, 111, 111, 247, 18, 171, 16, 36, 122, 78, 200, 209, 123, 76, 133,
            220, 124, 99, 221, 27, 160, 216, 235, 36, 212, 56, 197, 12, 79, 176, 201, 231, 206,
            161, 230, 233, 67, 137, 29, 86, 42, 95, 4, 201, 230, 123, 152, 89, 213, 98, 181, 38,
            203, 218, 20,
        ];
        let mul_scalar_le: [u8; 32] = [
            147, 154, 188, 106, 202, 232, 95, 242, 32, 41, 128, 61, 255, 159, 66, 107, 162, 120, 3,
            117, 225, 188, 223, 129, 91, 109, 37, 187, 151, 111, 192, 29,
        ];
        let expected_mul_le: [u8; 192] = [
            179, 97, 99, 239, 22, 225, 246, 148, 92, 98, 137, 164, 201, 221, 90, 38, 241, 191, 33,
            252, 238, 31, 240, 23, 13, 132, 61, 48, 250, 58, 240, 189, 82, 24, 135, 184, 5, 66, 31,
            13, 200, 63, 147, 141, 96, 82, 107, 6, 153, 181, 143, 123, 74, 33, 15, 100, 240, 111,
            213, 151, 19, 143, 92, 36, 81, 34, 251, 4, 214, 122, 104, 43, 138, 209, 9, 211, 121,
            41, 111, 33, 252, 174, 39, 202, 16, 254, 148, 188, 128, 38, 200, 26, 192, 88, 92, 10,
            189, 214, 207, 218, 117, 31, 144, 97, 94, 186, 53, 129, 53, 191, 228, 222, 169, 183,
            111, 74, 196, 243, 134, 51, 2, 77, 183, 162, 113, 38, 145, 39, 246, 63, 153, 185, 104,
            41, 133, 64, 4, 211, 11, 175, 184, 38, 76, 18, 182, 59, 76, 44, 11, 12, 201, 82, 219,
            87, 4, 33, 122, 208, 15, 250, 197, 212, 58, 149, 43, 84, 14, 161, 17, 146, 126, 223,
            175, 50, 206, 40, 103, 119, 59, 34, 41, 100, 233, 58, 182, 165, 156, 14, 114, 39, 251,
            20,
        ];

        let scalar_be_va = 0x100000000;
        let point_be_va = 0x200000000;
        let scalar_le_va = 0x300000000;
        let point_le_va = 0x400000000;
        let result_be_va = 0x500000000;
        let result_le_va = 0x600000000;

        let mut result_be_buf = [0u8; 192];
        let mut result_le_buf = [0u8; 192];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mul_scalar_be, scalar_be_va),
                MemoryRegion::new_readonly(&mul_point_be, point_be_va),
                MemoryRegion::new_writable(&mut result_be_buf, result_be_va),
                MemoryRegion::new_readonly(&mul_scalar_le, scalar_le_va),
                MemoryRegion::new_readonly(&mul_point_le, point_le_va),
                MemoryRegion::new_writable(&mut result_le_buf, result_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_multiply_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g2_multiply_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g2_multiply_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G2_BE,
            MUL,
            scalar_be_va,
            point_be_va,
            result_be_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_be_buf, expected_mul_be);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G2_LE,
            MUL,
            scalar_le_va,
            point_le_va,
            result_le_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_le_buf, expected_mul_le);
    }

    #[test]
    fn test_syscall_bls12_381_pairing_be() {
        use crate::bls12_381_curve_id::BLS12_381_BE;

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let g1_bytes: [u8; 96] = [
            3, 161, 104, 54, 242, 116, 16, 50, 15, 113, 42, 38, 108, 11, 127, 64, 43, 249, 50, 133,
            105, 8, 133, 238, 34, 6, 189, 119, 153, 36, 75, 65, 87, 249, 90, 109, 133, 200, 203,
            25, 127, 68, 251, 243, 14, 210, 204, 35, 18, 124, 149, 5, 68, 178, 57, 230, 253, 154,
            192, 163, 5, 146, 144, 100, 7, 102, 9, 76, 67, 251, 147, 45, 27, 111, 204, 213, 219,
            141, 58, 11, 235, 100, 6, 220, 77, 230, 232, 200, 210, 200, 3, 184, 10, 80, 23, 164,
        ];
        let g2_bytes: [u8; 192] = [
            8, 249, 218, 154, 232, 125, 250, 185, 153, 60, 132, 155, 188, 119, 50, 205, 32, 76,
            184, 181, 164, 158, 64, 12, 179, 181, 150, 95, 226, 9, 175, 51, 169, 185, 34, 178, 249,
            161, 27, 164, 210, 107, 171, 203, 246, 11, 158, 86, 14, 135, 197, 225, 7, 44, 94, 243,
            216, 200, 100, 199, 118, 14, 106, 181, 88, 202, 207, 156, 227, 101, 126, 236, 46, 189,
            238, 73, 220, 118, 151, 73, 255, 249, 103, 103, 255, 185, 91, 82, 212, 148, 110, 19,
            212, 111, 199, 197, 4, 144, 25, 145, 196, 142, 205, 252, 85, 85, 48, 243, 209, 62, 57,
            212, 44, 149, 81, 113, 171, 60, 193, 73, 40, 11, 36, 120, 19, 62, 2, 25, 22, 232, 227,
            50, 35, 75, 172, 205, 2, 37, 27, 65, 182, 6, 74, 43, 1, 239, 105, 129, 184, 98, 215,
            81, 15, 19, 171, 39, 252, 57, 176, 171, 181, 71, 124, 251, 53, 202, 213, 33, 58, 175,
            52, 41, 89, 230, 217, 177, 32, 24, 82, 166, 240, 232, 223, 24, 141, 70, 121, 25, 51,
            173, 30, 6,
        ];
        let expected_gt: [u8; 576] = [
            14, 57, 164, 128, 118, 229, 58, 194, 163, 179, 7, 155, 19, 27, 195, 184, 247, 246, 83,
            76, 63, 71, 120, 72, 143, 130, 2, 192, 35, 251, 36, 232, 229, 122, 68, 126, 54, 228,
            197, 249, 112, 234, 93, 130, 133, 246, 75, 41, 13, 31, 232, 225, 105, 219, 180, 105,
            225, 184, 43, 57, 184, 10, 228, 147, 245, 227, 40, 68, 215, 217, 15, 164, 14, 231, 119,
            134, 120, 33, 210, 52, 64, 47, 39, 42, 171, 221, 225, 58, 249, 247, 204, 161, 20, 16,
            103, 1, 0, 168, 109, 157, 223, 60, 147, 11, 76, 2, 95, 86, 174, 4, 100, 125, 124, 226,
            31, 159, 199, 160, 49, 98, 76, 124, 221, 101, 6, 213, 111, 44, 24, 172, 78, 42, 216,
            137, 91, 68, 211, 40, 210, 172, 242, 29, 115, 220, 11, 156, 249, 117, 118, 12, 59, 59,
            87, 137, 217, 190, 144, 62, 249, 103, 244, 247, 152, 112, 238, 31, 122, 136, 39, 9, 49,
            215, 22, 180, 164, 120, 166, 115, 62, 130, 4, 216, 57, 155, 8, 214, 116, 9, 222, 168,
            34, 242, 19, 47, 183, 124, 196, 222, 58, 135, 75, 97, 242, 231, 190, 238, 162, 50, 124,
            230, 229, 172, 156, 140, 196, 163, 213, 49, 153, 144, 167, 118, 122, 167, 70, 203, 145,
            120, 237, 46, 135, 130, 0, 204, 139, 61, 22, 10, 243, 232, 15, 38, 161, 146, 106, 138,
            86, 198, 8, 167, 229, 125, 95, 28, 120, 51, 23, 161, 250, 105, 125, 177, 169, 168, 97,
            5, 0, 231, 143, 141, 22, 92, 143, 148, 95, 66, 151, 154, 55, 169, 0, 91, 107, 5, 59,
            252, 8, 140, 0, 195, 64, 135, 197, 226, 235, 170, 127, 176, 217, 7, 180, 235, 222, 58,
            195, 221, 192, 130, 86, 143, 0, 199, 225, 53, 57, 181, 151, 152, 81, 183, 252, 251, 5,
            124, 61, 164, 133, 169, 14, 20, 206, 36, 56, 1, 197, 214, 23, 10, 32, 223, 128, 87,
            166, 33, 61, 29, 190, 90, 150, 82, 121, 109, 255, 211, 79, 46, 57, 48, 213, 125, 8, 93,
            10, 151, 162, 137, 133, 129, 237, 101, 77, 39, 85, 94, 234, 43, 85, 101, 240, 233, 93,
            57, 171, 13, 18, 38, 31, 29, 41, 169, 193, 49, 108, 119, 231, 130, 97, 45, 35, 252,
            149, 125, 116, 64, 163, 70, 40, 143, 160, 14, 15, 91, 168, 207, 77, 40, 74, 208, 114,
            50, 64, 119, 216, 182, 96, 218, 0, 185, 69, 105, 194, 103, 19, 129, 33, 204, 250, 237,
            191, 143, 122, 56, 234, 62, 8, 224, 1, 242, 110, 10, 194, 178, 198, 220, 151, 167, 234,
            235, 207, 148, 93, 249, 221, 153, 15, 86, 89, 76, 49, 29, 18, 74, 0, 246, 42, 143, 89,
            60, 48, 96, 23, 173, 209, 213, 156, 80, 154, 159, 161, 12, 178, 225, 226, 77, 99, 249,
            154, 246, 110, 96, 176, 79, 90, 2, 190, 63, 189, 123, 170, 206, 119, 142, 138, 15, 93,
            191, 230, 100, 159, 142, 50, 119, 204, 157, 201, 230, 93, 57, 3, 125, 96, 195, 247,
            195, 76, 24, 176, 99, 88, 206, 86, 63, 204, 37, 173, 182, 116, 51, 240, 15, 155, 199,
            199, 198, 183, 44, 241, 251, 236, 35, 178, 36, 8, 107, 82, 153, 144, 28, 29, 229, 150,
            157, 37, 216, 96, 116,
        ];

        let g1_va = 0x100000000;
        let g2_va = 0x200000000;
        let result_va = 0x300000000;

        let mut result_buf = [0u8; 576]; // GT size

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&g1_bytes, g1_va),
                MemoryRegion::new_readonly(&g2_bytes, g2_va),
                MemoryRegion::new_writable(&mut result_buf, result_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_one_pair_cost = invoke_context.get_execution_cost().bls12_381_one_pair_cost;
        invoke_context.mock_set_remaining(bls12_381_one_pair_cost);

        let result = SyscallCurvePairingMap::rust(
            &mut invoke_context,
            BLS12_381_BE,
            1,
            g1_va,
            g2_va,
            result_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_buf, expected_gt);
    }

    #[test]
    fn test_syscall_bls12_381_pairing_le() {
        use crate::bls12_381_curve_id::BLS12_381_LE;

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let g1_bytes: [u8; 96] = [
            35, 204, 210, 14, 243, 251, 68, 127, 25, 203, 200, 133, 109, 90, 249, 87, 65, 75, 36,
            153, 119, 189, 6, 34, 238, 133, 8, 105, 133, 50, 249, 43, 64, 127, 11, 108, 38, 42,
            113, 15, 50, 16, 116, 242, 54, 104, 161, 3, 164, 23, 80, 10, 184, 3, 200, 210, 200,
            232, 230, 77, 220, 6, 100, 235, 11, 58, 141, 219, 213, 204, 111, 27, 45, 147, 251, 67,
            76, 9, 102, 7, 100, 144, 146, 5, 163, 192, 154, 253, 230, 57, 178, 68, 5, 149, 124, 18,
        ];
        let g2_bytes: [u8; 192] = [
            197, 199, 111, 212, 19, 110, 148, 212, 82, 91, 185, 255, 103, 103, 249, 255, 73, 151,
            118, 220, 73, 238, 189, 46, 236, 126, 101, 227, 156, 207, 202, 88, 181, 106, 14, 118,
            199, 100, 200, 216, 243, 94, 44, 7, 225, 197, 135, 14, 86, 158, 11, 246, 203, 171, 107,
            210, 164, 27, 161, 249, 178, 34, 185, 169, 51, 175, 9, 226, 95, 150, 181, 179, 12, 64,
            158, 164, 181, 184, 76, 32, 205, 50, 119, 188, 155, 132, 60, 153, 185, 250, 125, 232,
            154, 218, 249, 8, 6, 30, 173, 51, 25, 121, 70, 141, 24, 223, 232, 240, 166, 82, 24, 32,
            177, 217, 230, 89, 41, 52, 175, 58, 33, 213, 202, 53, 251, 124, 71, 181, 171, 176, 57,
            252, 39, 171, 19, 15, 81, 215, 98, 184, 129, 105, 239, 1, 43, 74, 6, 182, 65, 27, 37,
            2, 205, 172, 75, 35, 50, 227, 232, 22, 25, 2, 62, 19, 120, 36, 11, 40, 73, 193, 60,
            171, 113, 81, 149, 44, 212, 57, 62, 209, 243, 48, 85, 85, 252, 205, 142, 196, 145, 25,
            144, 4,
        ];
        let expected_gt: [u8; 576] = [
            116, 96, 216, 37, 157, 150, 229, 29, 28, 144, 153, 82, 107, 8, 36, 178, 35, 236, 251,
            241, 44, 183, 198, 199, 199, 155, 15, 240, 51, 116, 182, 173, 37, 204, 63, 86, 206, 88,
            99, 176, 24, 76, 195, 247, 195, 96, 125, 3, 57, 93, 230, 201, 157, 204, 119, 50, 142,
            159, 100, 230, 191, 93, 15, 138, 142, 119, 206, 170, 123, 189, 63, 190, 2, 90, 79, 176,
            96, 110, 246, 154, 249, 99, 77, 226, 225, 178, 12, 161, 159, 154, 80, 156, 213, 209,
            173, 23, 96, 48, 60, 89, 143, 42, 246, 0, 74, 18, 29, 49, 76, 89, 86, 15, 153, 221,
            249, 93, 148, 207, 235, 234, 167, 151, 220, 198, 178, 194, 10, 110, 242, 1, 224, 8, 62,
            234, 56, 122, 143, 191, 237, 250, 204, 33, 129, 19, 103, 194, 105, 69, 185, 0, 218, 96,
            182, 216, 119, 64, 50, 114, 208, 74, 40, 77, 207, 168, 91, 15, 14, 160, 143, 40, 70,
            163, 64, 116, 125, 149, 252, 35, 45, 97, 130, 231, 119, 108, 49, 193, 169, 41, 29, 31,
            38, 18, 13, 171, 57, 93, 233, 240, 101, 85, 43, 234, 94, 85, 39, 77, 101, 237, 129,
            133, 137, 162, 151, 10, 93, 8, 125, 213, 48, 57, 46, 79, 211, 255, 109, 121, 82, 150,
            90, 190, 29, 61, 33, 166, 87, 128, 223, 32, 10, 23, 214, 197, 1, 56, 36, 206, 20, 14,
            169, 133, 164, 61, 124, 5, 251, 252, 183, 81, 152, 151, 181, 57, 53, 225, 199, 0, 143,
            86, 130, 192, 221, 195, 58, 222, 235, 180, 7, 217, 176, 127, 170, 235, 226, 197, 135,
            64, 195, 0, 140, 8, 252, 59, 5, 107, 91, 0, 169, 55, 154, 151, 66, 95, 148, 143, 92,
            22, 141, 143, 231, 0, 5, 97, 168, 169, 177, 125, 105, 250, 161, 23, 51, 120, 28, 95,
            125, 229, 167, 8, 198, 86, 138, 106, 146, 161, 38, 15, 232, 243, 10, 22, 61, 139, 204,
            0, 130, 135, 46, 237, 120, 145, 203, 70, 167, 122, 118, 167, 144, 153, 49, 213, 163,
            196, 140, 156, 172, 229, 230, 124, 50, 162, 238, 190, 231, 242, 97, 75, 135, 58, 222,
            196, 124, 183, 47, 19, 242, 34, 168, 222, 9, 116, 214, 8, 155, 57, 216, 4, 130, 62,
            115, 166, 120, 164, 180, 22, 215, 49, 9, 39, 136, 122, 31, 238, 112, 152, 247, 244,
            103, 249, 62, 144, 190, 217, 137, 87, 59, 59, 12, 118, 117, 249, 156, 11, 220, 115, 29,
            242, 172, 210, 40, 211, 68, 91, 137, 216, 42, 78, 172, 24, 44, 111, 213, 6, 101, 221,
            124, 76, 98, 49, 160, 199, 159, 31, 226, 124, 125, 100, 4, 174, 86, 95, 2, 76, 11, 147,
            60, 223, 157, 109, 168, 0, 1, 103, 16, 20, 161, 204, 247, 249, 58, 225, 221, 171, 42,
            39, 47, 64, 52, 210, 33, 120, 134, 119, 231, 14, 164, 15, 217, 215, 68, 40, 227, 245,
            147, 228, 10, 184, 57, 43, 184, 225, 105, 180, 219, 105, 225, 232, 31, 13, 41, 75, 246,
            133, 130, 93, 234, 112, 249, 197, 228, 54, 126, 68, 122, 229, 232, 36, 251, 35, 192, 2,
            130, 143, 72, 120, 71, 63, 76, 83, 246, 247, 184, 195, 27, 19, 155, 7, 179, 163, 194,
            58, 229, 118, 128, 164, 57, 14,
        ];

        let g1_va = 0x100000000;
        let g2_va = 0x200000000;
        let result_va = 0x300000000;

        let mut result_buf = [0u8; 576]; // GT size

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&g1_bytes, g1_va),
                MemoryRegion::new_readonly(&g2_bytes, g2_va),
                MemoryRegion::new_writable(&mut result_buf, result_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_one_pair_cost = invoke_context.get_execution_cost().bls12_381_one_pair_cost;
        invoke_context.mock_set_remaining(bls12_381_one_pair_cost);

        let result = SyscallCurvePairingMap::rust(
            &mut invoke_context,
            BLS12_381_LE,
            1,
            g1_va,
            g2_va,
            result_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_buf, expected_gt);
    }

    #[test]
    fn test_syscall_bls12_381_decompress_g1() {
        use crate::bls12_381_curve_id::{BLS12_381_G1_BE, BLS12_381_G1_LE};

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let compressed_be: [u8; 48] = [
            175, 159, 245, 68, 142, 96, 188, 154, 113, 143, 70, 58, 193, 2, 189, 111, 135, 114,
            230, 70, 12, 25, 7, 106, 108, 137, 213, 128, 110, 90, 142, 244, 75, 111, 59, 138, 240,
            158, 55, 164, 229, 100, 152, 122, 38, 185, 222, 218,
        ];
        let expected_affine_be: [u8; 96] = [
            15, 159, 245, 68, 142, 96, 188, 154, 113, 143, 70, 58, 193, 2, 189, 111, 135, 114, 230,
            70, 12, 25, 7, 106, 108, 137, 213, 128, 110, 90, 142, 244, 75, 111, 59, 138, 240, 158,
            55, 164, 229, 100, 152, 122, 38, 185, 222, 218, 18, 79, 1, 246, 62, 35, 162, 234, 146,
            109, 7, 85, 44, 104, 10, 250, 158, 31, 181, 244, 117, 193, 27, 53, 184, 79, 160, 237,
            168, 51, 41, 200, 58, 4, 107, 95, 246, 171, 241, 202, 120, 228, 135, 135, 100, 50, 123,
            58,
        ];
        let compressed_le: [u8; 48] = [
            218, 222, 185, 38, 122, 152, 100, 229, 164, 55, 158, 240, 138, 59, 111, 75, 244, 142,
            90, 110, 128, 213, 137, 108, 106, 7, 25, 12, 70, 230, 114, 135, 111, 189, 2, 193, 58,
            70, 143, 113, 154, 188, 96, 142, 68, 245, 159, 175,
        ];
        let expected_affine_le: [u8; 96] = [
            218, 222, 185, 38, 122, 152, 100, 229, 164, 55, 158, 240, 138, 59, 111, 75, 244, 142,
            90, 110, 128, 213, 137, 108, 106, 7, 25, 12, 70, 230, 114, 135, 111, 189, 2, 193, 58,
            70, 143, 113, 154, 188, 96, 142, 68, 245, 159, 15, 58, 123, 50, 100, 135, 135, 228,
            120, 202, 241, 171, 246, 95, 107, 4, 58, 200, 41, 51, 168, 237, 160, 79, 184, 53, 27,
            193, 117, 244, 181, 31, 158, 250, 10, 104, 44, 85, 7, 109, 146, 234, 162, 35, 62, 246,
            1, 79, 18,
        ];

        let input_be_va = 0x100000000;
        let result_be_va = 0x200000000;
        let input_le_va = 0x300000000;
        let result_le_va = 0x400000000;
        let mut result_be_buf = [0u8; 96];
        let mut result_le_buf = [0u8; 96];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&compressed_be, input_be_va),
                MemoryRegion::new_writable(&mut result_be_buf, result_be_va),
                MemoryRegion::new_readonly(&compressed_le, input_le_va),
                MemoryRegion::new_writable(&mut result_le_buf, result_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_decompress_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g2_decompress_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g2_decompress_cost);

        let result = SyscallCurveDecompress::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            input_be_va,
            result_be_va,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_be_buf, expected_affine_be);

        let result = SyscallCurveDecompress::rust(
            &mut invoke_context,
            BLS12_381_G1_LE,
            input_le_va,
            result_le_va,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_le_buf, expected_affine_le);
    }

    #[test]
    fn test_syscall_bls12_381_decompress_g2() {
        use crate::bls12_381_curve_id::{BLS12_381_G2_BE, BLS12_381_G2_LE};

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let compressed_be: [u8; 96] = [
            143, 106, 18, 220, 40, 152, 4, 228, 139, 35, 104, 146, 179, 74, 205, 172, 146, 137, 11,
            106, 74, 42, 135, 137, 53, 249, 64, 251, 173, 232, 48, 209, 125, 222, 13, 209, 121,
            238, 185, 179, 111, 105, 71, 223, 39, 48, 195, 104, 23, 24, 170, 59, 111, 106, 167, 51,
            231, 186, 224, 182, 172, 73, 15, 18, 211, 143, 59, 2, 115, 190, 196, 163, 111, 11, 36,
            133, 86, 96, 188, 135, 16, 37, 216, 175, 71, 182, 222, 31, 207, 155, 16, 255, 112, 78,
            242, 111,
        ];
        let expected_affine_be: [u8; 192] = [
            15, 106, 18, 220, 40, 152, 4, 228, 139, 35, 104, 146, 179, 74, 205, 172, 146, 137, 11,
            106, 74, 42, 135, 137, 53, 249, 64, 251, 173, 232, 48, 209, 125, 222, 13, 209, 121,
            238, 185, 179, 111, 105, 71, 223, 39, 48, 195, 104, 23, 24, 170, 59, 111, 106, 167, 51,
            231, 186, 224, 182, 172, 73, 15, 18, 211, 143, 59, 2, 115, 190, 196, 163, 111, 11, 36,
            133, 86, 96, 188, 135, 16, 37, 216, 175, 71, 182, 222, 31, 207, 155, 16, 255, 112, 78,
            242, 111, 11, 217, 244, 83, 201, 111, 182, 168, 171, 205, 183, 118, 199, 85, 130, 157,
            95, 69, 159, 126, 122, 27, 92, 84, 253, 147, 96, 176, 74, 57, 13, 228, 178, 111, 246,
            157, 74, 120, 174, 255, 146, 92, 32, 214, 164, 56, 206, 144, 13, 59, 111, 251, 170, 85,
            159, 219, 108, 187, 31, 15, 106, 176, 64, 191, 56, 77, 217, 87, 144, 196, 148, 21, 12,
            171, 99, 121, 128, 120, 187, 224, 192, 107, 104, 178, 75, 205, 118, 64, 234, 168, 214,
            11, 125, 153, 55, 5,
        ];
        let compressed_le: [u8; 96] = [
            111, 242, 78, 112, 255, 16, 155, 207, 31, 222, 182, 71, 175, 216, 37, 16, 135, 188, 96,
            86, 133, 36, 11, 111, 163, 196, 190, 115, 2, 59, 143, 211, 18, 15, 73, 172, 182, 224,
            186, 231, 51, 167, 106, 111, 59, 170, 24, 23, 104, 195, 48, 39, 223, 71, 105, 111, 179,
            185, 238, 121, 209, 13, 222, 125, 209, 48, 232, 173, 251, 64, 249, 53, 137, 135, 42,
            74, 106, 11, 137, 146, 172, 205, 74, 179, 146, 104, 35, 139, 228, 4, 152, 40, 220, 18,
            106, 143,
        ];
        let expected_affine_le: [u8; 192] = [
            111, 242, 78, 112, 255, 16, 155, 207, 31, 222, 182, 71, 175, 216, 37, 16, 135, 188, 96,
            86, 133, 36, 11, 111, 163, 196, 190, 115, 2, 59, 143, 211, 18, 15, 73, 172, 182, 224,
            186, 231, 51, 167, 106, 111, 59, 170, 24, 23, 104, 195, 48, 39, 223, 71, 105, 111, 179,
            185, 238, 121, 209, 13, 222, 125, 209, 48, 232, 173, 251, 64, 249, 53, 137, 135, 42,
            74, 106, 11, 137, 146, 172, 205, 74, 179, 146, 104, 35, 139, 228, 4, 152, 40, 220, 18,
            106, 15, 5, 55, 153, 125, 11, 214, 168, 234, 64, 118, 205, 75, 178, 104, 107, 192, 224,
            187, 120, 128, 121, 99, 171, 12, 21, 148, 196, 144, 87, 217, 77, 56, 191, 64, 176, 106,
            15, 31, 187, 108, 219, 159, 85, 170, 251, 111, 59, 13, 144, 206, 56, 164, 214, 32, 92,
            146, 255, 174, 120, 74, 157, 246, 111, 178, 228, 13, 57, 74, 176, 96, 147, 253, 84, 92,
            27, 122, 126, 159, 69, 95, 157, 130, 85, 199, 118, 183, 205, 171, 168, 182, 111, 201,
            83, 244, 217, 11,
        ];

        let input_be_va = 0x100000000;
        let result_be_va = 0x200000000;
        let input_le_va = 0x300000000;
        let result_le_va = 0x400000000;
        let mut result_be_buf = [0u8; 192];
        let mut result_le_buf = [0u8; 192];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&compressed_be, input_be_va),
                MemoryRegion::new_writable(&mut result_be_buf, result_be_va),
                MemoryRegion::new_readonly(&compressed_le, input_le_va),
                MemoryRegion::new_writable(&mut result_le_buf, result_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_decompress_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g2_decompress_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g2_decompress_cost);

        let result = SyscallCurveDecompress::rust(
            &mut invoke_context,
            BLS12_381_G2_BE,
            input_be_va,
            result_be_va,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_be_buf, expected_affine_be);

        let result = SyscallCurveDecompress::rust(
            &mut invoke_context,
            BLS12_381_G2_LE,
            input_le_va,
            result_le_va,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_le_buf, expected_affine_le);
    }

    #[test]
    fn test_syscall_bls12_381_validate_g1() {
        use crate::bls12_381_curve_id::{BLS12_381_G1_BE, BLS12_381_G1_LE};

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let point_bytes_be: [u8; 96] = [
            22, 163, 250, 67, 197, 168, 103, 201, 128, 33, 170, 96, 74, 40, 45, 90, 105, 181, 244,
            124, 128, 107, 27, 142, 158, 96, 0, 46, 144, 27, 61, 205, 65, 38, 141, 165, 55, 113,
            114, 23, 36, 105, 252, 115, 147, 16, 12, 39, 11, 19, 53, 215, 107, 128, 94, 68, 22, 46,
            74, 179, 236, 232, 220, 30, 48, 169, 85, 16, 70, 112, 26, 37, 73, 104, 203, 189, 42,
            96, 141, 90, 167, 41, 61, 82, 184, 80, 93, 112, 204, 140, 225, 245, 103, 130, 184, 194,
        ];

        let point_bytes_le: [u8; 96] = [
            39, 12, 16, 147, 115, 252, 105, 36, 23, 114, 113, 55, 165, 141, 38, 65, 205, 61, 27,
            144, 46, 0, 96, 158, 142, 27, 107, 128, 124, 244, 181, 105, 90, 45, 40, 74, 96, 170,
            33, 128, 201, 103, 168, 197, 67, 250, 163, 22, 194, 184, 130, 103, 245, 225, 140, 204,
            112, 93, 80, 184, 82, 61, 41, 167, 90, 141, 96, 42, 189, 203, 104, 73, 37, 26, 112, 70,
            16, 85, 169, 48, 30, 220, 232, 236, 179, 74, 46, 22, 68, 94, 128, 107, 215, 53, 19, 11,
        ];

        let point_be_va = 0x100000000;
        let point_le_va = 0x200000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&point_bytes_be, point_be_va),
                MemoryRegion::new_readonly(&point_bytes_le, point_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g1_validate_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g1_validate_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g1_validate_cost);

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            point_be_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            BLS12_381_G1_LE,
            point_le_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
    }

    #[test]
    fn test_syscall_bls12_381_validate_g2() {
        use crate::bls12_381_curve_id::{BLS12_381_G2_BE, BLS12_381_G2_LE};

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let point_bytes_be: [u8; 192] = [
            0, 79, 207, 115, 91, 72, 0, 80, 49, 59, 203, 189, 178, 240, 18, 141, 223, 147, 62, 79,
            98, 131, 147, 33, 103, 151, 137, 12, 160, 13, 78, 180, 13, 221, 89, 239, 178, 249, 141,
            8, 38, 137, 23, 71, 213, 2, 28, 13, 24, 168, 51, 6, 34, 184, 228, 22, 173, 11, 224,
            168, 14, 103, 154, 18, 166, 51, 255, 154, 45, 230, 253, 149, 145, 16, 251, 107, 248,
            55, 53, 150, 37, 131, 133, 138, 156, 195, 70, 202, 131, 144, 166, 164, 80, 251, 179,
            167, 8, 54, 188, 153, 10, 235, 83, 14, 211, 95, 212, 54, 120, 175, 148, 83, 253, 106,
            53, 178, 157, 118, 208, 110, 0, 187, 111, 14, 140, 246, 139, 200, 205, 178, 72, 36, 67,
            140, 39, 100, 163, 104, 140, 78, 91, 123, 130, 197, 12, 176, 70, 104, 65, 43, 104, 232,
            102, 238, 229, 115, 253, 62, 61, 207, 116, 223, 245, 206, 250, 163, 30, 200, 76, 101,
            93, 69, 216, 240, 189, 198, 253, 27, 199, 32, 215, 224, 12, 50, 78, 204, 106, 40, 117,
            68, 44, 113,
        ];

        let point_bytes_le: [u8; 192] = [
            167, 179, 251, 80, 164, 166, 144, 131, 202, 70, 195, 156, 138, 133, 131, 37, 150, 53,
            55, 248, 107, 251, 16, 145, 149, 253, 230, 45, 154, 255, 51, 166, 18, 154, 103, 14,
            168, 224, 11, 173, 22, 228, 184, 34, 6, 51, 168, 24, 13, 28, 2, 213, 71, 23, 137, 38,
            8, 141, 249, 178, 239, 89, 221, 13, 180, 78, 13, 160, 12, 137, 151, 103, 33, 147, 131,
            98, 79, 62, 147, 223, 141, 18, 240, 178, 189, 203, 59, 49, 80, 0, 72, 91, 115, 207, 79,
            0, 113, 44, 68, 117, 40, 106, 204, 78, 50, 12, 224, 215, 32, 199, 27, 253, 198, 189,
            240, 216, 69, 93, 101, 76, 200, 30, 163, 250, 206, 245, 223, 116, 207, 61, 62, 253,
            115, 229, 238, 102, 232, 104, 43, 65, 104, 70, 176, 12, 197, 130, 123, 91, 78, 140,
            104, 163, 100, 39, 140, 67, 36, 72, 178, 205, 200, 139, 246, 140, 14, 111, 187, 0, 110,
            208, 118, 157, 178, 53, 106, 253, 83, 148, 175, 120, 54, 212, 95, 211, 14, 83, 235, 10,
            153, 188, 54, 8,
        ];

        let point_be_va = 0x100000000;
        let point_le_va = 0x200000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&point_bytes_be, point_be_va),
                MemoryRegion::new_readonly(&point_bytes_le, point_le_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_validate_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g2_validate_cost;
        invoke_context.mock_set_remaining(2 * bls12_381_g2_validate_cost);

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            BLS12_381_G2_BE,
            point_be_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            BLS12_381_G2_LE,
            point_le_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
    }
}
