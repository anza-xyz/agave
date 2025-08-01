#[derive(Clone, Copy, Default)]
pub struct SVMFeatureSet {
    pub move_precompile_verification_to_svm: bool,
    pub remove_accounts_executable_flag_checks: bool,
    pub bpf_account_data_direct_mapping: bool,
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
    pub loosen_cpi_size_restriction: bool,
    pub increase_tx_account_lock_limit: bool,
    pub enable_extend_program_checked: bool,
    pub formalize_loaded_transaction_data_size: bool,
    pub disable_zk_elgamal_proof_program: bool,
    pub reenable_zk_elgamal_proof_program: bool,
    pub raise_cpi_nesting_limit_to_8: bool,
}

impl SVMFeatureSet {
    pub fn all_enabled() -> Self {
        Self {
            move_precompile_verification_to_svm: true,
            remove_accounts_executable_flag_checks: true,
            bpf_account_data_direct_mapping: true,
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
            loosen_cpi_size_restriction: true,
            increase_tx_account_lock_limit: true,
            enable_extend_program_checked: true,
            formalize_loaded_transaction_data_size: true,
            disable_zk_elgamal_proof_program: true,
            reenable_zk_elgamal_proof_program: true,
            raise_cpi_nesting_limit_to_8: true,
        }
    }
}
