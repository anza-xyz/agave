#[derive(Clone, Copy, Default)]
pub struct SVMFeatureSet {
    pub move_precompile_verification_to_svm: bool,
    pub stricter_abi_and_runtime_constraints: bool,
    pub account_data_direct_mapping: bool,
    pub enable_bpf_loader_set_authority_checked_ix: bool,
    pub enable_loader_v4: bool,
    pub deplete_cu_meter_on_vm_failure: bool,
    pub abort_on_invalid_curve: bool,
    pub blake3_syscall_enabled: bool,
    pub curve25519_syscall_enabled: bool,
    pub disable_deploy_of_alloc_free_syscall: bool,
    pub disable_fees_sysvar: bool,
    pub disable_sbpf_v0_execution: bool,
    pub enable_alt_bn128_compression_syscall: bool,
    pub enable_alt_bn128_syscall: bool,
    pub enable_big_mod_exp_syscall: bool,
    pub enable_get_epoch_stake_syscall: bool,
    pub enable_poseidon_syscall: bool,
    pub enable_sbpf_v1_deployment_and_execution: bool,
    pub enable_sbpf_v2_deployment_and_execution: bool,
    pub enable_sbpf_v3_deployment_and_execution: bool,
    pub get_sysvar_syscall_enabled: bool,
    pub last_restart_slot_sysvar: bool,
    pub reenable_sbpf_v0_execution: bool,
    pub remaining_compute_units_syscall_enabled: bool,
    pub remove_bpf_loader_incorrect_program_id: bool,
    pub move_stake_and_move_lamports_ixs: bool,
    pub stake_raise_minimum_delegation_to_1_sol: bool,
    pub deprecate_legacy_vote_ixs: bool,
    pub mask_out_rent_epoch_in_vm_serialization: bool,
    pub simplify_alt_bn128_syscall_error_codes: bool,
    pub fix_alt_bn128_multiplication_input_length: bool,
    pub increase_tx_account_lock_limit: bool,
    pub enable_extend_program_checked: bool,
    pub formalize_loaded_transaction_data_size: bool,
    pub disable_zk_elgamal_proof_program: bool,
    pub reenable_zk_elgamal_proof_program: bool,
    pub raise_cpi_nesting_limit_to_8: bool,
    pub provide_instruction_data_offset_in_vm_r2: bool,
    pub vote_state_v4: bool,
}

impl SVMFeatureSet {
    pub fn all_enabled() -> Self {
        Self {
            move_precompile_verification_to_svm: true,
            stricter_abi_and_runtime_constraints: true,
            account_data_direct_mapping: true,
            enable_bpf_loader_set_authority_checked_ix: true,
            enable_loader_v4: true,
            deplete_cu_meter_on_vm_failure: true,
            abort_on_invalid_curve: true,
            blake3_syscall_enabled: true,
            curve25519_syscall_enabled: true,
            disable_deploy_of_alloc_free_syscall: true,
            disable_fees_sysvar: true,
            disable_sbpf_v0_execution: true,
            enable_alt_bn128_compression_syscall: true,
            enable_alt_bn128_syscall: true,
            enable_big_mod_exp_syscall: true,
            enable_get_epoch_stake_syscall: true,
            enable_poseidon_syscall: true,
            enable_sbpf_v1_deployment_and_execution: true,
            enable_sbpf_v2_deployment_and_execution: true,
            enable_sbpf_v3_deployment_and_execution: true,
            get_sysvar_syscall_enabled: true,
            last_restart_slot_sysvar: true,
            reenable_sbpf_v0_execution: true,
            remaining_compute_units_syscall_enabled: true,
            remove_bpf_loader_incorrect_program_id: true,
            move_stake_and_move_lamports_ixs: true,
            stake_raise_minimum_delegation_to_1_sol: true,
            deprecate_legacy_vote_ixs: true,
            mask_out_rent_epoch_in_vm_serialization: true,
            simplify_alt_bn128_syscall_error_codes: true,
            fix_alt_bn128_multiplication_input_length: true,
            increase_tx_account_lock_limit: true,
            enable_extend_program_checked: true,
            formalize_loaded_transaction_data_size: true,
            disable_zk_elgamal_proof_program: true,
            reenable_zk_elgamal_proof_program: true,
            raise_cpi_nesting_limit_to_8: true,
            provide_instruction_data_offset_in_vm_r2: true,
            vote_state_v4: true,
        }
    }

    /// Converts the feature set into a bitmask.
    #[allow(clippy::identity_op)]
    #[inline]
    pub const fn to_bitmask(&self) -> u64 {
        (self.move_precompile_verification_to_svm as u64) << 0
            | (self.stricter_abi_and_runtime_constraints as u64) << 1
            | (self.account_data_direct_mapping as u64) << 2
            | (self.enable_bpf_loader_set_authority_checked_ix as u64) << 3
            | (self.enable_loader_v4 as u64) << 4
            | (self.deplete_cu_meter_on_vm_failure as u64) << 5
            | (self.abort_on_invalid_curve as u64) << 6
            | (self.blake3_syscall_enabled as u64) << 7
            | (self.curve25519_syscall_enabled as u64) << 8
            | (self.disable_deploy_of_alloc_free_syscall as u64) << 9
            | (self.disable_fees_sysvar as u64) << 10
            | (self.disable_sbpf_v0_execution as u64) << 11
            | (self.enable_alt_bn128_compression_syscall as u64) << 12
            | (self.enable_alt_bn128_syscall as u64) << 13
            | (self.enable_big_mod_exp_syscall as u64) << 14
            | (self.enable_get_epoch_stake_syscall as u64) << 15
            | (self.enable_poseidon_syscall as u64) << 16
            | (self.enable_sbpf_v1_deployment_and_execution as u64) << 17
            | (self.enable_sbpf_v2_deployment_and_execution as u64) << 18
            | (self.enable_sbpf_v3_deployment_and_execution as u64) << 19
            | (self.get_sysvar_syscall_enabled as u64) << 20
            | (self.last_restart_slot_sysvar as u64) << 21
            | (self.reenable_sbpf_v0_execution as u64) << 22
            | (self.remaining_compute_units_syscall_enabled as u64) << 23
            | (self.remove_bpf_loader_incorrect_program_id as u64) << 24
            | (self.move_stake_and_move_lamports_ixs as u64) << 25
            | (self.stake_raise_minimum_delegation_to_1_sol as u64) << 26
            | (self.deprecate_legacy_vote_ixs as u64) << 27
            | (self.mask_out_rent_epoch_in_vm_serialization as u64) << 28
            | (self.simplify_alt_bn128_syscall_error_codes as u64) << 29
            | (self.fix_alt_bn128_multiplication_input_length as u64) << 30
            | (self.increase_tx_account_lock_limit as u64) << 31
            | (self.enable_extend_program_checked as u64) << 32
            | (self.formalize_loaded_transaction_data_size as u64) << 33
            | (self.disable_zk_elgamal_proof_program as u64) << 34
            | (self.reenable_zk_elgamal_proof_program as u64) << 35
            | (self.raise_cpi_nesting_limit_to_8 as u64) << 36
            | (self.provide_instruction_data_offset_in_vm_r2 as u64) << 37
            | (self.vote_state_v4 as u64) << 38
    }
}
