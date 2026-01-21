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

// NOTE: These constants are temporarily added here to avoid immediate
// dependency conflicts.
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
    /// Elliptic Curve Point Decompression
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
                PodG1Point as PodBLSG1Point, PodG2Point as PodBLSG2Point,
                PodG1Compressed as PodBLSG1Compressed, PodG2Compressed as
                PodBLSG2Compressed
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
                    _ => Ok(1),
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
                    _ => Ok(1),
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
    /// Elliptic Curve Pairing Map
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
                    .bls12_381_pair_base_cost
                    .saturating_add(
                        execution_cost
                            .bls12_381_pair_per_pair_cost
                            .saturating_mul(num_pairs),
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

    #[test]
    fn test_syscall_bls12_381_g1_add() {
        use {
            crate::bls12_381_curve_id::BLS12_381_G1_BE,
            solana_curve25519::curve_syscall_traits::ADD,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let p1_bytes: [u8; 96] = [
            9, 86, 169, 212, 236, 245, 17, 101, 127, 183, 56, 13, 99, 100, 183, 133, 57, 107, 96,
            220, 198, 197, 2, 215, 225, 175, 212, 57, 168, 143, 104, 127, 117, 242, 180, 200, 162,
            135, 72, 155, 88, 154, 58, 90, 58, 46, 248, 176, 10, 206, 25, 112, 240, 1, 57, 89, 10,
            30, 165, 94, 164, 252, 219, 225, 133, 214, 161, 4, 118, 177, 123, 53, 57, 53, 233, 255,
            112, 117, 241, 247, 185, 195, 232, 36, 123, 31, 221, 6, 57, 176, 251, 163, 195, 39, 35,
            175,
        ];
        let p2_bytes: [u8; 96] = [
            13, 32, 61, 215, 83, 124, 186, 189, 82, 0, 79, 244, 67, 167, 21, 50, 48, 229, 8, 107,
            51, 15, 19, 47, 75, 77, 246, 185, 63, 66, 143, 109, 237, 211, 153, 146, 163, 175, 74,
            69, 50, 198, 235, 218, 9, 170, 225, 46, 22, 211, 116, 84, 32, 115, 130, 224, 106, 250,
            205, 143, 238, 115, 74, 207, 238, 193, 232, 16, 59, 140, 20, 252, 7, 34, 144, 47, 137,
            56, 190, 170, 235, 189, 238, 45, 97, 58, 199, 202, 45, 164, 139, 200, 190, 215, 9, 59,
        ];
        let expected_sum: [u8; 96] = [
            23, 62, 255, 137, 157, 188, 98, 86, 192, 102, 136, 171, 187, 49, 155, 83, 204, 133,
            217, 144, 137, 103, 15, 4, 116, 75, 127, 65, 29, 89, 223, 147, 32, 161, 91, 104, 96,
            211, 239, 102, 233, 95, 48, 130, 207, 154, 19, 189, 18, 112, 102, 145, 36, 73, 17, 27,
            47, 96, 116, 45, 56, 25, 16, 191, 56, 21, 86, 216, 133, 245, 207, 71, 158, 31, 29, 51,
            84, 185, 134, 138, 64, 68, 55, 161, 55, 153, 214, 155, 250, 21, 233, 4, 3, 117, 41,
            239,
        ];

        let p1_va = 0x100000000;
        let p2_va = 0x200000000;
        let result_va = 0x300000000;

        let mut result_buf = [0u8; 96];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&p1_bytes, p1_va),
                MemoryRegion::new_readonly(&p2_bytes, p2_va),
                MemoryRegion::new_writable(&mut result_buf, result_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g1_add_cost = invoke_context.get_execution_cost().bls12_381_g1_add_cost;
        invoke_context.mock_set_remaining(bls12_381_g1_add_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            ADD,
            p1_va,
            p2_va,
            result_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_buf, expected_sum);
    }

    #[test]
    fn test_syscall_bls12_381_g1_sub() {
        use {
            crate::bls12_381_curve_id::BLS12_381_G1_BE,
            solana_curve25519::curve_syscall_traits::SUB,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let sub_p1: [u8; 96] = [
            6, 126, 67, 177, 221, 168, 219, 147, 17, 32, 109, 112, 204, 95, 207, 179, 227, 202, 32,
            250, 118, 43, 195, 105, 176, 47, 188, 43, 181, 226, 123, 119, 132, 240, 97, 172, 225,
            247, 180, 76, 58, 229, 188, 121, 247, 28, 245, 198, 17, 128, 94, 239, 206, 10, 10, 20,
            148, 186, 226, 202, 12, 196, 71, 72, 167, 44, 87, 64, 24, 214, 238, 218, 6, 166, 113,
            165, 178, 8, 221, 0, 21, 154, 72, 160, 158, 70, 46, 244, 127, 4, 250, 158, 31, 2, 130,
            152,
        ];
        let sub_p2: [u8; 96] = [
            12, 173, 131, 106, 17, 172, 169, 46, 205, 228, 83, 25, 204, 216, 118, 223, 16, 102, 52,
            235, 202, 255, 183, 91, 99, 78, 141, 169, 14, 244, 161, 28, 240, 32, 214, 46, 0, 93,
            106, 73, 41, 176, 220, 160, 251, 37, 18, 110, 15, 86, 67, 210, 137, 114, 71, 220, 167,
            121, 177, 224, 142, 151, 152, 29, 206, 12, 35, 6, 46, 60, 53, 127, 84, 78, 231, 88, 49,
            95, 219, 36, 224, 182, 0, 253, 136, 115, 59, 15, 80, 229, 136, 103, 27, 211, 120, 90,
        ];
        let expected_sub: [u8; 96] = [
            13, 144, 131, 116, 67, 229, 136, 165, 135, 146, 181, 191, 197, 215, 68, 126, 103, 158,
            231, 50, 49, 105, 8, 243, 53, 209, 99, 16, 39, 177, 211, 99, 128, 164, 37, 101, 139,
            186, 14, 225, 84, 210, 120, 16, 203, 115, 160, 49, 10, 243, 68, 241, 87, 193, 186, 179,
            87, 214, 88, 39, 123, 126, 136, 31, 178, 134, 203, 222, 127, 206, 218, 240, 135, 183,
            93, 145, 136, 148, 174, 238, 159, 0, 117, 212, 171, 247, 148, 197, 206, 7, 225, 81,
            114, 74, 63, 201,
        ];

        let p1_va = 0x100000000;
        let p2_va = 0x200000000;
        let result_va = 0x300000000;

        let mut result_buf = [0u8; 96];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&sub_p1, p1_va),
                MemoryRegion::new_readonly(&sub_p2, p2_va),
                MemoryRegion::new_writable(&mut result_buf, result_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g1_subtract_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g1_subtract_cost;
        invoke_context.mock_set_remaining(bls12_381_g1_subtract_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            SUB,
            p1_va,
            p2_va,
            result_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_buf, expected_sub);
    }

    #[test]
    fn test_syscall_bls12_381_g1_mul() {
        use {
            crate::bls12_381_curve_id::BLS12_381_G1_BE,
            solana_curve25519::curve_syscall_traits::MUL,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let mul_point: [u8; 96] = [
            20, 18, 233, 201, 110, 206, 56, 32, 8, 44, 140, 121, 37, 196, 157, 56, 180, 134, 164,
            33, 180, 130, 147, 7, 26, 239, 183, 163, 219, 85, 143, 197, 247, 243, 117, 252, 201,
            171, 156, 90, 210, 7, 43, 92, 89, 130, 165, 224, 5, 101, 24, 54, 189, 22, 73, 76, 145,
            136, 99, 59, 51, 255, 124, 43, 61, 8, 121, 30, 118, 90, 254, 12, 126, 92, 152, 78, 44,
            231, 126, 56, 220, 35, 54, 117, 2, 175, 190, 105, 138, 188, 202, 36, 171, 12, 231, 225,
        ];
        let mul_scalar: [u8; 32] = [
            29, 192, 111, 151, 187, 37, 109, 91, 129, 223, 188, 225, 117, 3, 120, 162, 107, 66,
            159, 255, 61, 128, 41, 32, 242, 95, 232, 202, 106, 188, 154, 147,
        ];
        let expected_mul: [u8; 96] = [
            22, 101, 72, 255, 3, 247, 39, 218, 234, 117, 208, 91, 158, 114, 126, 55, 166, 71, 227,
            205, 6, 124, 55, 255, 167, 66, 154, 237, 83, 143, 8, 179, 98, 185, 162, 164, 170, 62,
            141, 4, 1, 179, 41, 49, 95, 212, 139, 227, 18, 125, 245, 10, 169, 201, 171, 172, 152,
            1, 105, 81, 159, 160, 252, 184, 80, 59, 165, 170, 185, 114, 248, 208, 228, 111, 229,
            200, 221, 204, 9, 120, 153, 142, 88, 240, 228, 164, 157, 79, 72, 55, 119, 239, 56, 104,
            54, 58,
        ];

        let scalar_va = 0x100000000;
        let point_va = 0x200000000;
        let result_va = 0x300000000;

        let mut result_buf = [0u8; 96];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mul_scalar, scalar_va),
                MemoryRegion::new_readonly(&mul_point, point_va),
                MemoryRegion::new_writable(&mut result_buf, result_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g1_multiply_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g1_multiply_cost;
        invoke_context.mock_set_remaining(bls12_381_g1_multiply_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            MUL,
            scalar_va,
            point_va,
            result_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_buf, expected_mul);
    }

    #[test]
    fn test_syscall_bls12_381_g2_add() {
        use {
            crate::bls12_381_curve_id::BLS12_381_G2_BE,
            solana_curve25519::curve_syscall_traits::ADD,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let p1_bytes: [u8; 192] = [
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
        let p2_bytes: [u8; 192] = [
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
        let expected_sum: [u8; 192] = [
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

        let p1_va = 0x100000000;
        let p2_va = 0x200000000;
        let result_va = 0x300000000;

        let mut result_buf = [0u8; 192];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&p1_bytes, p1_va),
                MemoryRegion::new_readonly(&p2_bytes, p2_va),
                MemoryRegion::new_writable(&mut result_buf, result_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_add_cost = invoke_context.get_execution_cost().bls12_381_g2_add_cost;
        invoke_context.mock_set_remaining(bls12_381_g2_add_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G2_BE,
            ADD,
            p1_va,
            p2_va,
            result_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_buf, expected_sum);
    }

    #[test]
    fn test_syscall_bls12_381_g2_sub() {
        use {
            crate::bls12_381_curve_id::BLS12_381_G2_BE,
            solana_curve25519::curve_syscall_traits::SUB,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let sub_p1: [u8; 192] = [
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
        let sub_p2: [u8; 192] = [
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
        let expected_sub: [u8; 192] = [
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

        let p1_va = 0x100000000;
        let p2_va = 0x200000000;
        let result_va = 0x300000000;

        let mut result_buf = [0u8; 192];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&sub_p1, p1_va),
                MemoryRegion::new_readonly(&sub_p2, p2_va),
                MemoryRegion::new_writable(&mut result_buf, result_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_subtract_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g2_subtract_cost;
        invoke_context.mock_set_remaining(bls12_381_g2_subtract_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G2_BE,
            SUB,
            p1_va,
            p2_va,
            result_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_buf, expected_sub);
    }

    #[test]
    fn test_syscall_bls12_381_g2_mul() {
        use {
            crate::bls12_381_curve_id::BLS12_381_G2_BE,
            solana_curve25519::curve_syscall_traits::MUL,
        };

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;
        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set);

        let mul_point: [u8; 192] = [
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
        let mul_scalar: [u8; 32] = [
            29, 192, 111, 151, 187, 37, 109, 91, 129, 223, 188, 225, 117, 3, 120, 162, 107, 66,
            159, 255, 61, 128, 41, 32, 242, 95, 232, 202, 106, 188, 154, 147,
        ];
        let expected_mul: [u8; 192] = [
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

        let scalar_va = 0x100000000;
        let point_va = 0x200000000;
        let result_va = 0x300000000;

        let mut result_buf = [0u8; 192];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mul_scalar, scalar_va),
                MemoryRegion::new_readonly(&mul_point, point_va),
                MemoryRegion::new_writable(&mut result_buf, result_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_multiply_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g2_multiply_cost;
        invoke_context.mock_set_remaining(bls12_381_g2_multiply_cost);

        let result = SyscallCurveGroupOps::rust(
            &mut invoke_context,
            BLS12_381_G2_BE,
            MUL,
            scalar_va,
            point_va,
            result_va,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_buf, expected_mul);
    }

    #[test]
    fn test_syscall_bls12_381_pairing() {
        use crate::bls12_381_curve_id::BLS12_381_BE;

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let g1_bytes: [u8; 96] = [
            14, 109, 179, 165, 174, 150, 53, 65, 12, 65, 27, 45, 194, 213, 151, 119, 31, 9, 194,
            218, 211, 97, 143, 55, 132, 10, 10, 7, 104, 36, 23, 203, 71, 135, 56, 38, 9, 165, 177,
            68, 248, 225, 230, 200, 147, 186, 63, 233, 21, 191, 133, 37, 160, 52, 2, 249, 115, 111,
            217, 16, 135, 98, 243, 12, 166, 50, 1, 39, 253, 40, 48, 101, 13, 108, 76, 237, 12, 58,
            245, 113, 75, 90, 47, 216, 156, 35, 223, 160, 84, 160, 16, 120, 6, 103, 132, 45,
        ];
        let g2_bytes: [u8; 192] = [
            15, 28, 53, 78, 205, 37, 201, 37, 205, 66, 121, 138, 56, 120, 211, 146, 62, 157, 236,
            102, 230, 5, 117, 56, 119, 245, 49, 92, 180, 132, 179, 13, 134, 141, 118, 41, 212, 62,
            195, 177, 51, 52, 191, 208, 220, 201, 90, 107, 13, 181, 66, 54, 203, 86, 69, 137, 40,
            184, 192, 93, 191, 235, 165, 80, 4, 38, 89, 135, 14, 18, 234, 249, 67, 193, 188, 223,
            62, 250, 122, 158, 179, 30, 56, 95, 176, 0, 239, 3, 155, 141, 171, 224, 6, 198, 246,
            205, 6, 159, 251, 49, 151, 133, 155, 57, 7, 59, 182, 22, 210, 145, 203, 209, 62, 59,
            188, 64, 10, 195, 114, 199, 159, 219, 25, 179, 2, 23, 18, 62, 254, 101, 113, 141, 109,
            175, 167, 124, 251, 150, 153, 231, 34, 234, 175, 52, 24, 151, 67, 45, 189, 197, 212,
            143, 102, 25, 192, 143, 11, 184, 18, 229, 147, 145, 193, 201, 144, 5, 38, 33, 182, 18,
            92, 144, 146, 21, 74, 61, 179, 104, 196, 158, 244, 63, 172, 73, 22, 70, 100, 202, 90,
            139, 102, 98,
        ];
        let expected_gt: [u8; 576] = [
            15, 124, 219, 251, 80, 222, 126, 43, 196, 49, 34, 25, 153, 233, 75, 211, 42, 192, 212,
            47, 166, 9, 195, 137, 10, 35, 193, 182, 166, 62, 203, 209, 114, 113, 49, 238, 59, 132,
            83, 7, 201, 153, 94, 103, 238, 38, 46, 65, 24, 238, 46, 54, 251, 113, 78, 88, 118, 210,
            42, 173, 49, 235, 34, 119, 182, 188, 84, 157, 108, 155, 133, 112, 91, 220, 108, 130,
            37, 245, 120, 246, 152, 201, 30, 73, 98, 226, 186, 91, 221, 33, 124, 121, 240, 115,
            217, 139, 0, 76, 30, 151, 238, 121, 241, 41, 110, 37, 191, 196, 47, 131, 231, 124, 230,
            162, 114, 111, 209, 71, 210, 58, 12, 203, 213, 51, 134, 225, 104, 113, 58, 71, 229, 65,
            151, 115, 104, 25, 44, 57, 243, 39, 102, 13, 144, 200, 15, 88, 112, 64, 33, 170, 119,
            93, 19, 156, 108, 29, 198, 39, 166, 48, 23, 74, 155, 223, 42, 172, 198, 112, 219, 243,
            55, 173, 86, 90, 91, 200, 75, 132, 111, 74, 52, 155, 78, 78, 103, 251, 35, 177, 14, 75,
            131, 163, 7, 89, 200, 246, 231, 130, 166, 139, 189, 229, 65, 8, 248, 174, 254, 151, 73,
            215, 90, 237, 189, 155, 173, 55, 74, 177, 209, 235, 254, 87, 18, 175, 120, 153, 71,
            112, 51, 104, 126, 107, 29, 240, 156, 107, 44, 244, 49, 210, 9, 255, 160, 66, 243, 94,
            2, 213, 65, 92, 123, 138, 42, 60, 163, 203, 26, 228, 174, 224, 119, 252, 51, 37, 21,
            211, 101, 222, 41, 123, 220, 123, 206, 62, 235, 209, 132, 71, 145, 101, 86, 192, 65,
            97, 4, 64, 112, 128, 20, 188, 225, 66, 2, 121, 250, 44, 15, 111, 14, 46, 90, 145, 158,
            141, 164, 140, 221, 75, 157, 244, 85, 6, 108, 104, 222, 142, 17, 157, 236, 111, 67, 41,
            237, 242, 44, 9, 83, 142, 146, 85, 204, 94, 111, 124, 233, 150, 16, 243, 161, 196, 153,
            36, 162, 27, 203, 69, 247, 195, 31, 24, 69, 138, 191, 107, 221, 159, 51, 24, 170, 218,
            132, 249, 30, 160, 130, 230, 3, 251, 36, 125, 152, 249, 16, 96, 56, 14, 37, 1, 56, 146,
            210, 78, 201, 45, 11, 77, 198, 95, 103, 57, 51, 124, 247, 176, 78, 84, 148, 48, 135,
            203, 66, 216, 134, 116, 101, 52, 136, 60, 221, 177, 111, 145, 41, 95, 184, 75, 145, 7,
            209, 62, 33, 132, 45, 53, 254, 247, 34, 115, 154, 24, 142, 146, 8, 203, 106, 160, 213,
            238, 51, 215, 213, 212, 4, 27, 35, 214, 79, 198, 97, 99, 93, 255, 88, 169, 38, 41, 253,
            166, 173, 203, 111, 138, 3, 145, 135, 250, 220, 58, 135, 173, 142, 37, 244, 251, 227,
            36, 184, 214, 100, 121, 14, 100, 53, 57, 225, 209, 242, 136, 154, 24, 95, 240, 144,
            111, 180, 30, 215, 146, 64, 189, 2, 26, 90, 224, 86, 231, 207, 151, 147, 232, 57, 219,
            32, 135, 0, 91, 207, 185, 15, 72, 146, 240, 12, 212, 172, 147, 185, 17, 1, 180, 121,
            38, 146, 36, 24, 48, 50, 240, 230, 140, 143, 243, 205, 247, 103, 162, 166, 14, 130, 18,
            69, 54, 148, 184, 201, 27, 25, 5, 131, 197, 244, 196, 7, 104, 245, 12, 32, 24, 183,
            219, 165, 214, 144, 163, 9, 157,
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

        let bls12_381_pair_base_cost = invoke_context.get_execution_cost().bls12_381_pair_base_cost;
        let bls12_381_pair_per_pair_cost = invoke_context
            .get_execution_cost()
            .bls12_381_pair_per_pair_cost;
        let bls12_381_pair_cost = bls12_381_pair_base_cost
            .checked_add(bls12_381_pair_per_pair_cost)
            .unwrap();
        invoke_context.mock_set_remaining(bls12_381_pair_cost);

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
    fn test_syscall_bls12_381_decompress() {
        use crate::bls12_381_curve_id::BLS12_381_G1_BE;

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let compressed: [u8; 48] = [
            175, 159, 245, 68, 142, 96, 188, 154, 113, 143, 70, 58, 193, 2, 189, 111, 135, 114,
            230, 70, 12, 25, 7, 106, 108, 137, 213, 128, 110, 90, 142, 244, 75, 111, 59, 138, 240,
            158, 55, 164, 229, 100, 152, 122, 38, 185, 222, 218,
        ];
        let expected_affine: [u8; 96] = [
            15, 159, 245, 68, 142, 96, 188, 154, 113, 143, 70, 58, 193, 2, 189, 111, 135, 114, 230,
            70, 12, 25, 7, 106, 108, 137, 213, 128, 110, 90, 142, 244, 75, 111, 59, 138, 240, 158,
            55, 164, 229, 100, 152, 122, 38, 185, 222, 218, 18, 79, 1, 246, 62, 35, 162, 234, 146,
            109, 7, 85, 44, 104, 10, 250, 158, 31, 181, 244, 117, 193, 27, 53, 184, 79, 160, 237,
            168, 51, 41, 200, 58, 4, 107, 95, 246, 171, 241, 202, 120, 228, 135, 135, 100, 50, 123,
            58,
        ];

        let input_va = 0x100000000;
        let result_va = 0x200000000;
        let mut result_buf = [0u8; 96];

        let mut memory_mapping = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&compressed, input_va),
                MemoryRegion::new_writable(&mut result_buf, result_va),
            ],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_decompress_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g2_decompress_cost;
        invoke_context.mock_set_remaining(bls12_381_g2_decompress_cost);

        let result = SyscallCurveDecompress::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            input_va,
            result_va,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
        assert_eq!(result_buf, expected_affine);
    }

    #[test]
    fn test_syscall_bls12_381_validate() {
        use crate::bls12_381_curve_id::BLS12_381_G1_BE;

        let config = Config::default();
        let feature_set = SVMFeatureSet {
            enable_bls12_381_syscall: true,
            ..Default::default()
        };
        let feature_set = &feature_set;

        prepare_mock_with_feature_set!(invoke_context, program_id, bpf_loader::id(), feature_set,);

        let point_bytes: [u8; 96] = [
            22, 163, 250, 67, 197, 168, 103, 201, 128, 33, 170, 96, 74, 40, 45, 90, 105, 181, 244,
            124, 128, 107, 27, 142, 158, 96, 0, 46, 144, 27, 61, 205, 65, 38, 141, 165, 55, 113,
            114, 23, 36, 105, 252, 115, 147, 16, 12, 39, 11, 19, 53, 215, 107, 128, 94, 68, 22, 46,
            74, 179, 236, 232, 220, 30, 48, 169, 85, 16, 70, 112, 26, 37, 73, 104, 203, 189, 42,
            96, 141, 90, 167, 41, 61, 82, 184, 80, 93, 112, 204, 140, 225, 245, 103, 130, 184, 194,
        ];
        let point_va = 0x100000000;

        let mut memory_mapping = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(&point_bytes, point_va)],
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let bls12_381_g2_validate_cost = invoke_context
            .get_execution_cost()
            .bls12_381_g2_validate_cost;
        invoke_context.mock_set_remaining(bls12_381_g2_validate_cost);

        let result = SyscallCurvePointValidation::rust(
            &mut invoke_context,
            BLS12_381_G1_BE,
            point_va,
            0,
            0,
            0,
            &mut memory_mapping,
        );

        assert_eq!(0, result.unwrap());
    }
}
