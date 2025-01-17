//! This module is only for syscall definitions that bring in no extra dependencies.
#[allow(unused)]
use crate::codes::*;
use crate::define_syscall;

define_syscall!(fn sol_secp256k1_recover(hash: *const u8, recovery_id: u64, signature: *const u8, result: *mut u8) -> u64, SOL_SECP256K1_RECOVER);
define_syscall!(fn sol_poseidon(parameters: u64, endianness: u64, vals: *const u8, val_len: u64, hash_result: *mut u8) -> u64, SOL_POSEIDON);
define_syscall!(fn sol_invoke_signed_c(instruction_addr: *const u8, account_infos_addr: *const u8, account_infos_len: u64, signers_seeds_addr: *const u8, signers_seeds_len: u64) -> u64, SOL_INVOKE_SIGNED_C);
define_syscall!(fn sol_invoke_signed_rust(instruction_addr: *const u8, account_infos_addr: *const u8, account_infos_len: u64, signers_seeds_addr: *const u8, signers_seeds_len: u64) -> u64, SOL_INVOKE_SIGNED_RUST);
define_syscall!(fn sol_set_return_data(data: *const u8, length: u64), SOL_SET_RETURN_DATA);
define_syscall!(fn sol_get_stack_height() -> u64, SOL_GET_STACK_HEIGHT);
define_syscall!(fn sol_log_(message: *const u8, len: u64), SOL_LOG_);
define_syscall!(fn sol_log_64_(arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64), SOL_LOG_64_);
define_syscall!(fn sol_log_compute_units_(), SOL_LOG_COMPUTE_UNITS_);
define_syscall!(fn sol_log_data(data: *const u8, data_len: u64), SOL_LOG_DATA);
define_syscall!(fn sol_memcpy_(dst: *mut u8, src: *const u8, n: u64), SOL_MEMCPY_);
define_syscall!(fn sol_memmove_(dst: *mut u8, src: *const u8, n: u64), SOL_MEMMOVE_);
define_syscall!(fn sol_memcmp_(s1: *const u8, s2: *const u8, n: u64, result: *mut i32), SOL_MEMCMP_);
define_syscall!(fn sol_memset_(s: *mut u8, c: u8, n: u64), SOL_MEMSET_);
define_syscall!(fn sol_log_pubkey(pubkey_addr: *const u8), SOL_LOG_PUBKEY);
define_syscall!(fn sol_create_program_address(seeds_addr: *const u8, seeds_len: u64, program_id_addr: *const u8, address_bytes_addr: *const u8) -> u64, SOL_CREATE_PROGRAM_ADDRESS);
define_syscall!(fn sol_try_find_program_address(seeds_addr: *const u8, seeds_len: u64, program_id_addr: *const u8, address_bytes_addr: *const u8, bump_seed_addr: *const u8) -> u64, SOL_TRY_FIND_PROGRAM_ADDRESS);
define_syscall!(fn sol_sha256(vals: *const u8, val_len: u64, hash_result: *mut u8) -> u64, SOL_SHA256);
define_syscall!(fn sol_keccak256(vals: *const u8, val_len: u64, hash_result: *mut u8) -> u64, SOL_KECCAK256);
define_syscall!(fn sol_blake3(vals: *const u8, val_len: u64, hash_result: *mut u8) -> u64, SOL_BLAKE3);
define_syscall!(fn sol_curve_validate_point(curve_id: u64, point_addr: *const u8, result: *mut u8) -> u64, SOL_CURVE_VALIDATE_POINT);
define_syscall!(fn sol_curve_group_op(curve_id: u64, group_op: u64, left_input_addr: *const u8, right_input_addr: *const u8, result_point_addr: *mut u8) -> u64, SOL_CURVE_GROUP_OP);
define_syscall!(fn sol_curve_multiscalar_mul(curve_id: u64, scalars_addr: *const u8, points_addr: *const u8, points_len: u64, result_point_addr: *mut u8) -> u64, SOL_CURVE_MULTISCALAR_MUL);
define_syscall!(fn sol_curve_pairing_map(curve_id: u64, point: *const u8, result: *mut u8) -> u64, SOL_CURVE_PAIRING_MAP);
define_syscall!(fn sol_alt_bn128_group_op(group_op: u64, input: *const u8, input_size: u64, result: *mut u8) -> u64, SOL_ALT_BN128_GROUP_OP);
define_syscall!(fn sol_big_mod_exp(params: *const u8, result: *mut u8) -> u64, SOL_BIG_MOD_EXP);
define_syscall!(fn sol_remaining_compute_units() -> u64, SOL_REMAINING_COMPUTE_UNITS);
define_syscall!(fn sol_alt_bn128_compression(op: u64, input: *const u8, input_size: u64, result: *mut u8) -> u64, SOL_ALT_BN128_COMPRESSION);
define_syscall!(fn sol_get_sysvar(sysvar_id_addr: *const u8, result: *mut u8, offset: u64, length: u64) -> u64, SOL_GET_SYSVAR);
define_syscall!(fn sol_get_epoch_stake(vote_address: *const u8) -> u64, SOL_GET_EPOCH_STAKE);

// these are to be deprecated once they are superceded by sol_get_sysvar
define_syscall!(fn sol_get_clock_sysvar(addr: *mut u8) -> u64, SOL_GET_CLOCK_SYSVAR);
define_syscall!(fn sol_get_epoch_schedule_sysvar(addr: *mut u8) -> u64, SOL_GET_EPOCH_SCHEDULE_SYSVAR);
define_syscall!(fn sol_get_rent_sysvar(addr: *mut u8) -> u64, SOL_GET_RENT_SYSVAR);
define_syscall!(fn sol_get_last_restart_slot(addr: *mut u8) -> u64, SOL_GET_LAST_RESTART_SLOT);
define_syscall!(fn sol_get_epoch_rewards_sysvar(addr: *mut u8) -> u64, SOL_GET_EPOCH_REWARDS_SYSVAR);

// this cannot go through sol_get_sysvar but can be removed once no longer in use
define_syscall!(fn sol_get_fees_sysvar(addr: *mut u8) -> u64, SOL_GET_FEES_SYSVAR);
