pub use solana_program_runtime::execution_budget::{
    SVMTransactionExecutionBudget, SVMTransactionExecutionCost, MAX_CALL_DEPTH,
    MAX_INSTRUCTION_STACK_DEPTH, STACK_FRAME_SIZE,
};
use {
    solana_fee_structure::FeeDetails,
    solana_program_runtime::execution_budget::SVMTransactionExecutionAndFeeBudgetLimits,
    std::num::NonZeroU32,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComputeBudget {
    /// Number of compute units that a transaction or individual instruction is
    /// allowed to consume. Compute units are consumed by program execution,
    /// resources they use, etc...
    pub compute_unit_limit: u64,
    /// Number of compute units consumed by a log_u64 call
    pub log_64_units: u64,
    /// Number of compute units consumed by a create_program_address call
    pub create_program_address_units: u64,
    /// Number of compute units consumed by an invoke call (not including the cost incurred by
    /// the called program)
    pub invoke_units: u64,
    /// Maximum program instruction invocation stack depth. Invocation stack
    /// depth starts at 1 for transaction instructions and the stack depth is
    /// incremented each time a program invokes an instruction and decremented
    /// when a program returns.
    pub max_instruction_stack_depth: usize,
    /// Maximum cross-program invocation and instructions per transaction
    pub max_instruction_trace_length: usize,
    /// Base number of compute units consumed to call SHA256
    pub sha256_base_cost: u64,
    /// Incremental number of units consumed by SHA256 (based on bytes)
    pub sha256_byte_cost: u64,
    /// Maximum number of slices hashed per syscall
    pub sha256_max_slices: u64,
    /// Maximum SBF to BPF call depth
    pub max_call_depth: usize,
    /// Size of a stack frame in bytes, must match the size specified in the LLVM SBF backend
    pub stack_frame_size: usize,
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
    /// program heap region size, default: solana_program_entrypoint::HEAP_LENGTH
    pub heap_size: u32,
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

#[cfg(feature = "dev-context-only-utils")]
impl Default for ComputeBudget {
    fn default() -> Self {
        Self::from_budget_and_cost(
            &SVMTransactionExecutionBudget::default(),
            &SVMTransactionExecutionCost::default(),
        )
    }
}

impl ComputeBudget {
    pub fn new_with_defaults(simd_0268_active: bool, simd_0339_active: bool) -> Self {
        Self::from_budget_and_cost(
            &SVMTransactionExecutionBudget::new_with_defaults(simd_0268_active),
            &SVMTransactionExecutionCost::new_with_defaults(simd_0339_active),
        )
    }

    pub fn from_budget_and_cost(
        budget: &SVMTransactionExecutionBudget,
        cost: &SVMTransactionExecutionCost,
    ) -> Self {
        Self {
            compute_unit_limit: budget.compute_unit_limit,
            log_64_units: cost.log_64_units,
            create_program_address_units: cost.create_program_address_units,
            invoke_units: cost.invoke_units,
            max_instruction_stack_depth: budget.max_instruction_stack_depth,
            max_instruction_trace_length: budget.max_instruction_trace_length,
            sha256_base_cost: cost.sha256_base_cost,
            sha256_byte_cost: cost.sha256_byte_cost,
            sha256_max_slices: budget.sha256_max_slices,
            max_call_depth: budget.max_call_depth,
            stack_frame_size: budget.stack_frame_size,
            log_pubkey_units: cost.log_pubkey_units,
            cpi_bytes_per_unit: cost.cpi_bytes_per_unit,
            sysvar_base_cost: cost.sysvar_base_cost,
            secp256k1_recover_cost: cost.secp256k1_recover_cost,
            syscall_base_cost: cost.syscall_base_cost,
            curve25519_edwards_validate_point_cost: cost.curve25519_edwards_validate_point_cost,
            curve25519_edwards_add_cost: cost.curve25519_edwards_add_cost,
            curve25519_edwards_subtract_cost: cost.curve25519_edwards_subtract_cost,
            curve25519_edwards_multiply_cost: cost.curve25519_edwards_multiply_cost,
            curve25519_edwards_msm_base_cost: cost.curve25519_edwards_msm_base_cost,
            curve25519_edwards_msm_incremental_cost: cost.curve25519_edwards_msm_incremental_cost,
            curve25519_ristretto_validate_point_cost: cost.curve25519_ristretto_validate_point_cost,
            curve25519_ristretto_add_cost: cost.curve25519_ristretto_add_cost,
            curve25519_ristretto_subtract_cost: cost.curve25519_ristretto_subtract_cost,
            curve25519_ristretto_multiply_cost: cost.curve25519_ristretto_multiply_cost,
            curve25519_ristretto_msm_base_cost: cost.curve25519_ristretto_msm_base_cost,
            curve25519_ristretto_msm_incremental_cost: cost
                .curve25519_ristretto_msm_incremental_cost,
            heap_size: budget.heap_size,
            heap_cost: cost.heap_cost,
            mem_op_base_cost: cost.mem_op_base_cost,
            alt_bn128_addition_cost: cost.alt_bn128_addition_cost,
            alt_bn128_multiplication_cost: cost.alt_bn128_multiplication_cost,
            alt_bn128_pairing_one_pair_cost_first: cost.alt_bn128_pairing_one_pair_cost_first,
            alt_bn128_pairing_one_pair_cost_other: cost.alt_bn128_pairing_one_pair_cost_other,
            big_modular_exponentiation_base_cost: cost.big_modular_exponentiation_base_cost,
            big_modular_exponentiation_cost_divisor: cost.big_modular_exponentiation_cost_divisor,
            poseidon_cost_coefficient_a: cost.poseidon_cost_coefficient_a,
            poseidon_cost_coefficient_c: cost.poseidon_cost_coefficient_c,
            get_remaining_compute_units_cost: cost.get_remaining_compute_units_cost,
            alt_bn128_g1_compress: cost.alt_bn128_g1_compress,
            alt_bn128_g1_decompress: cost.alt_bn128_g1_decompress,
            alt_bn128_g2_compress: cost.alt_bn128_g2_compress,
            alt_bn128_g2_decompress: cost.alt_bn128_g2_decompress,
        }
    }

    pub fn to_budget(&self) -> SVMTransactionExecutionBudget {
        SVMTransactionExecutionBudget {
            compute_unit_limit: self.compute_unit_limit,
            max_instruction_stack_depth: self.max_instruction_stack_depth,
            max_instruction_trace_length: self.max_instruction_trace_length,
            sha256_max_slices: self.sha256_max_slices,
            max_call_depth: self.max_call_depth,
            stack_frame_size: self.stack_frame_size,
            heap_size: self.heap_size,
        }
    }

    pub fn to_cost(&self) -> SVMTransactionExecutionCost {
        SVMTransactionExecutionCost {
            log_64_units: self.log_64_units,
            create_program_address_units: self.create_program_address_units,
            invoke_units: self.invoke_units,
            sha256_base_cost: self.sha256_base_cost,
            sha256_byte_cost: self.sha256_byte_cost,
            log_pubkey_units: self.log_pubkey_units,
            cpi_bytes_per_unit: self.cpi_bytes_per_unit,
            sysvar_base_cost: self.sysvar_base_cost,
            secp256k1_recover_cost: self.secp256k1_recover_cost,
            syscall_base_cost: self.syscall_base_cost,
            curve25519_edwards_validate_point_cost: self.curve25519_edwards_validate_point_cost,
            curve25519_edwards_add_cost: self.curve25519_edwards_add_cost,
            curve25519_edwards_subtract_cost: self.curve25519_edwards_subtract_cost,
            curve25519_edwards_multiply_cost: self.curve25519_edwards_multiply_cost,
            curve25519_edwards_msm_base_cost: self.curve25519_edwards_msm_base_cost,
            curve25519_edwards_msm_incremental_cost: self.curve25519_edwards_msm_incremental_cost,
            curve25519_ristretto_validate_point_cost: self.curve25519_ristretto_validate_point_cost,
            curve25519_ristretto_add_cost: self.curve25519_ristretto_add_cost,
            curve25519_ristretto_subtract_cost: self.curve25519_ristretto_subtract_cost,
            curve25519_ristretto_multiply_cost: self.curve25519_ristretto_multiply_cost,
            curve25519_ristretto_msm_base_cost: self.curve25519_ristretto_msm_base_cost,
            curve25519_ristretto_msm_incremental_cost: self
                .curve25519_ristretto_msm_incremental_cost,
            heap_cost: self.heap_cost,
            mem_op_base_cost: self.mem_op_base_cost,
            alt_bn128_addition_cost: self.alt_bn128_addition_cost,
            alt_bn128_multiplication_cost: self.alt_bn128_multiplication_cost,
            alt_bn128_pairing_one_pair_cost_first: self.alt_bn128_pairing_one_pair_cost_first,
            alt_bn128_pairing_one_pair_cost_other: self.alt_bn128_pairing_one_pair_cost_other,
            big_modular_exponentiation_base_cost: self.big_modular_exponentiation_base_cost,
            big_modular_exponentiation_cost_divisor: self.big_modular_exponentiation_cost_divisor,
            poseidon_cost_coefficient_a: self.poseidon_cost_coefficient_a,
            poseidon_cost_coefficient_c: self.poseidon_cost_coefficient_c,
            get_remaining_compute_units_cost: self.get_remaining_compute_units_cost,
            alt_bn128_g1_compress: self.alt_bn128_g1_compress,
            alt_bn128_g1_decompress: self.alt_bn128_g1_decompress,
            alt_bn128_g2_compress: self.alt_bn128_g2_compress,
            alt_bn128_g2_decompress: self.alt_bn128_g2_decompress,
        }
    }

    pub fn get_compute_budget_and_limits(
        &self,
        loaded_accounts_data_size_limit: NonZeroU32,
        fee_details: FeeDetails,
    ) -> SVMTransactionExecutionAndFeeBudgetLimits {
        SVMTransactionExecutionAndFeeBudgetLimits {
            budget: self.to_budget(),
            loaded_accounts_data_size_limit,
            fee_details,
        }
    }
}
