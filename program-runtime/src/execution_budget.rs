use {
    solana_fee_structure::FeeDetails, solana_program_entrypoint::HEAP_LENGTH,
    solana_transaction_context::MAX_INSTRUCTION_TRACE_LENGTH, std::num::NonZeroU32,
};

/// Max instruction stack depth. This is the maximum nesting of instructions that can happen during
/// a transaction.
pub const MAX_INSTRUCTION_STACK_DEPTH: usize = 5;
/// Max instruction stack depth with SIMD-0268 enabled. Allows 8 nested CPIs.
pub const MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268: usize = 9;

fn get_max_instruction_stack_depth(simd_0268_active: bool) -> usize {
    if simd_0268_active {
        MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268
    } else {
        MAX_INSTRUCTION_STACK_DEPTH
    }
}

//Default CPI invocation cost
pub const DEFAULT_INVOCATION_COST: u64 = 1000;
//CPI Invocation cost with SIMD-0339 active
pub const INVOKE_UNITS_COST_SIMD_0339: u64 = 946;

fn get_invoke_unit_cost(simd_0339_active: bool) -> u64 {
    if simd_0339_active {
        INVOKE_UNITS_COST_SIMD_0339
    } else {
        DEFAULT_INVOCATION_COST
    }
}

/// Max call depth. This is the maximum nesting of SBF to SBF call that can happen within a program.
pub const MAX_CALL_DEPTH: usize = 64;

/// The size of one SBF stack frame.
pub const STACK_FRAME_SIZE: usize = 4096;

pub const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// Roughly 0.5us/page, where page is 32K; given roughly 15CU/us, the
/// default heap page cost = 0.5 * 15 ~= 8CU/page
pub const DEFAULT_HEAP_COST: u64 = 8;
pub const DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT: u32 = 200_000;
// SIMD-170 defines max CUs to be allocated for any builtin program instructions, that
// have not been migrated to sBPF programs.
pub const MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT: u32 = 3_000;
pub const MAX_HEAP_FRAME_BYTES: u32 = 256 * 1024;
pub const MIN_HEAP_FRAME_BYTES: u32 = HEAP_LENGTH as u32;

/// The total accounts data a transaction can load is limited to 64MiB to not break
/// anyone in Mainnet-beta today. It can be set by set_loaded_accounts_data_size_limit instruction
pub const MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES: NonZeroU32 =
    NonZeroU32::new(64 * 1024 * 1024).unwrap();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SVMTransactionExecutionBudget {
    /// Number of compute units that a transaction or individual instruction is
    /// allowed to consume. Compute units are consumed by program execution,
    /// resources they use, etc...
    pub compute_unit_limit: u64,
    /// Maximum program instruction invocation stack depth. Invocation stack
    /// depth starts at 1 for transaction instructions and the stack depth is
    /// incremented each time a program invokes an instruction and decremented
    /// when a program returns.
    pub max_instruction_stack_depth: usize,
    /// Maximum cross-program invocation and instructions per transaction
    pub max_instruction_trace_length: usize,
    /// Maximum number of slices hashed per syscall
    pub sha256_max_slices: u64,
    /// Maximum SBF to BPF call depth
    pub max_call_depth: usize,
    /// Size of a stack frame in bytes, must match the size specified in the LLVM SBF backend
    pub stack_frame_size: usize,
    /// program heap region size, default: solana_program_entrypoint::HEAP_LENGTH
    pub heap_size: u32,
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for SVMTransactionExecutionBudget {
    fn default() -> Self {
        Self::new_with_defaults(/* simd_0268_active */ false)
    }
}

impl SVMTransactionExecutionBudget {
    pub fn new_with_defaults(simd_0268_active: bool) -> Self {
        SVMTransactionExecutionBudget {
            compute_unit_limit: u64::from(MAX_COMPUTE_UNIT_LIMIT),
            max_instruction_stack_depth: get_max_instruction_stack_depth(simd_0268_active),
            max_instruction_trace_length: MAX_INSTRUCTION_TRACE_LENGTH,
            sha256_max_slices: 20_000,
            max_call_depth: MAX_CALL_DEPTH,
            stack_frame_size: STACK_FRAME_SIZE,
            heap_size: u32::try_from(solana_program_entrypoint::HEAP_LENGTH).unwrap(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SVMTransactionExecutionCost {
    /// Number of compute units consumed by a log_u64 call
    pub log_64_units: u64,
    /// Number of compute units consumed by a create_program_address call
    pub create_program_address_units: u64,
    /// Number of compute units consumed by an invoke call (not including the cost incurred by
    /// the called program)
    pub invoke_units: u64,
    /// Base number of compute units consumed to call SHA256
    pub sha256_base_cost: u64,
    /// Incremental number of units consumed by SHA256 (based on bytes)
    pub sha256_byte_cost: u64,
    /// Number of compute units consumed by logging a `Pubkey`
    pub log_pubkey_units: u64,
    /// Number of account data bytes per compute unit charged during a cross-program invocation
    pub cpi_bytes_per_unit: u64,
    /// Base number of compute units consumed to get a sysvar
    pub sysvar_base_cost: u64,
    /// Number of compute units consumed to call secp256k1_recover
    pub secp256k1_recover_cost: u64,
    /// Number of compute units consumed to do a syscall without any work
    pub syscall_base_cost: u64,
    /// Number of compute units consumed to validate a curve25519 edwards point
    pub curve25519_edwards_validate_point_cost: u64,
    /// Number of compute units consumed to add two curve25519 edwards points
    pub curve25519_edwards_add_cost: u64,
    /// Number of compute units consumed to subtract two curve25519 edwards points
    pub curve25519_edwards_subtract_cost: u64,
    /// Number of compute units consumed to multiply a curve25519 edwards point
    pub curve25519_edwards_multiply_cost: u64,
    /// Number of compute units consumed for a multiscalar multiplication (msm) of edwards points.
    /// The total cost is calculated as `msm_base_cost + (length - 1) * msm_incremental_cost`.
    pub curve25519_edwards_msm_base_cost: u64,
    /// Number of compute units consumed for a multiscalar multiplication (msm) of edwards points.
    /// The total cost is calculated as `msm_base_cost + (length - 1) * msm_incremental_cost`.
    pub curve25519_edwards_msm_incremental_cost: u64,
    /// Number of compute units consumed to validate a curve25519 ristretto point
    pub curve25519_ristretto_validate_point_cost: u64,
    /// Number of compute units consumed to add two curve25519 ristretto points
    pub curve25519_ristretto_add_cost: u64,
    /// Number of compute units consumed to subtract two curve25519 ristretto points
    pub curve25519_ristretto_subtract_cost: u64,
    /// Number of compute units consumed to multiply a curve25519 ristretto point
    pub curve25519_ristretto_multiply_cost: u64,
    /// Number of compute units consumed for a multiscalar multiplication (msm) of ristretto points.
    /// The total cost is calculated as `msm_base_cost + (length - 1) * msm_incremental_cost`.
    pub curve25519_ristretto_msm_base_cost: u64,
    /// Number of compute units consumed for a multiscalar multiplication (msm) of ristretto points.
    /// The total cost is calculated as `msm_base_cost + (length - 1) * msm_incremental_cost`.
    pub curve25519_ristretto_msm_incremental_cost: u64,
    /// Number of compute units per additional 32k heap above the default (~.5
    /// us per 32k at 15 units/us rounded up)
    pub heap_cost: u64,
    /// Memory operation syscall base cost
    pub mem_op_base_cost: u64,
    /// Number of compute units consumed to call alt_bn128_addition
    pub alt_bn128_addition_cost: u64,
    /// Number of compute units consumed to call alt_bn128_multiplication.
    pub alt_bn128_multiplication_cost: u64,
    /// Total cost will be alt_bn128_pairing_one_pair_cost_first
    /// + alt_bn128_pairing_one_pair_cost_other * (num_elems - 1)
    pub alt_bn128_pairing_one_pair_cost_first: u64,
    pub alt_bn128_pairing_one_pair_cost_other: u64,
    /// Big integer modular exponentiation base cost
    pub big_modular_exponentiation_base_cost: u64,
    /// Big integer moduler exponentiation cost divisor
    /// The modular exponentiation cost is computed as
    /// `input_length`/`big_modular_exponentiation_cost_divisor` + `big_modular_exponentiation_base_cost`
    pub big_modular_exponentiation_cost_divisor: u64,
    /// Coefficient `a` of the quadratic function which determines the number
    /// of compute units consumed to call poseidon syscall for a given number
    /// of inputs.
    pub poseidon_cost_coefficient_a: u64,
    /// Coefficient `c` of the quadratic function which determines the number
    /// of compute units consumed to call poseidon syscall for a given number
    /// of inputs.
    pub poseidon_cost_coefficient_c: u64,
    /// Number of compute units consumed for accessing the remaining compute units.
    pub get_remaining_compute_units_cost: u64,
    /// Number of compute units consumed to call alt_bn128_g1_compress.
    pub alt_bn128_g1_compress: u64,
    /// Number of compute units consumed to call alt_bn128_g1_decompress.
    pub alt_bn128_g1_decompress: u64,
    /// Number of compute units consumed to call alt_bn128_g2_compress.
    pub alt_bn128_g2_compress: u64,
    /// Number of compute units consumed to call alt_bn128_g2_decompress.
    pub alt_bn128_g2_decompress: u64,
}

impl Default for SVMTransactionExecutionCost {
    fn default() -> Self {
        Self::new_with_defaults(/* simd_0339_active */ false)
    }
}

impl SVMTransactionExecutionCost {
    pub fn new_with_defaults(simd_0339_active: bool) -> Self {
        SVMTransactionExecutionCost {
            log_64_units: 100,
            create_program_address_units: 1500,
            invoke_units: get_invoke_unit_cost(simd_0339_active),
            sha256_base_cost: 85,
            sha256_byte_cost: 1,
            log_pubkey_units: 100,
            cpi_bytes_per_unit: 250, // ~50MB at 200,000 units
            sysvar_base_cost: 100,
            secp256k1_recover_cost: 25_000,
            syscall_base_cost: 100,
            curve25519_edwards_validate_point_cost: 159,
            curve25519_edwards_add_cost: 473,
            curve25519_edwards_subtract_cost: 475,
            curve25519_edwards_multiply_cost: 2_177,
            curve25519_edwards_msm_base_cost: 2_273,
            curve25519_edwards_msm_incremental_cost: 758,
            curve25519_ristretto_validate_point_cost: 169,
            curve25519_ristretto_add_cost: 521,
            curve25519_ristretto_subtract_cost: 519,
            curve25519_ristretto_multiply_cost: 2_208,
            curve25519_ristretto_msm_base_cost: 2303,
            curve25519_ristretto_msm_incremental_cost: 788,
            heap_cost: DEFAULT_HEAP_COST,
            mem_op_base_cost: 10,
            alt_bn128_addition_cost: 334,
            alt_bn128_multiplication_cost: 3_840,
            alt_bn128_pairing_one_pair_cost_first: 36_364,
            alt_bn128_pairing_one_pair_cost_other: 12_121,
            big_modular_exponentiation_base_cost: 190,
            big_modular_exponentiation_cost_divisor: 2,
            poseidon_cost_coefficient_a: 61,
            poseidon_cost_coefficient_c: 542,
            get_remaining_compute_units_cost: 100,
            alt_bn128_g1_compress: 30,
            alt_bn128_g1_decompress: 398,
            alt_bn128_g2_compress: 86,
            alt_bn128_g2_decompress: 13610,
        }
    }

    /// Returns cost of the Poseidon hash function for the given number of
    /// inputs is determined by the following quadratic function:
    ///
    /// 61*n^2 + 542
    ///
    /// Which approximates the results of benchmarks of light-posiedon
    /// library[0]. These results assume 1 CU per 33 ns. Examples:
    ///
    /// * 1 input
    ///   * light-poseidon benchmark: `18,303 / 33 ≈ 555`
    ///   * function: `61*1^2 + 542 = 603`
    /// * 2 inputs
    ///   * light-poseidon benchmark: `25,866 / 33 ≈ 784`
    ///   * function: `61*2^2 + 542 = 786`
    /// * 3 inputs
    ///   * light-poseidon benchmark: `37,549 / 33 ≈ 1,138`
    ///   * function; `61*3^2 + 542 = 1091`
    ///
    /// [0] https://github.com/Lightprotocol/light-poseidon#performance
    pub fn poseidon_cost(&self, nr_inputs: u64) -> Option<u64> {
        let squared_inputs = nr_inputs.checked_pow(2)?;
        let mul_result = self
            .poseidon_cost_coefficient_a
            .checked_mul(squared_inputs)?;
        let final_result = mul_result.checked_add(self.poseidon_cost_coefficient_c)?;

        Some(final_result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SVMTransactionExecutionAndFeeBudgetLimits {
    pub budget: SVMTransactionExecutionBudget,
    pub loaded_accounts_data_size_limit: NonZeroU32,
    pub fee_details: FeeDetails,
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for SVMTransactionExecutionAndFeeBudgetLimits {
    fn default() -> Self {
        Self {
            budget: SVMTransactionExecutionBudget::default(),
            loaded_accounts_data_size_limit: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            fee_details: FeeDetails::default(),
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl SVMTransactionExecutionAndFeeBudgetLimits {
    pub fn with_fee(fee_details: FeeDetails) -> Self {
        Self {
            fee_details,
            ..SVMTransactionExecutionAndFeeBudgetLimits::default()
        }
    }
}
