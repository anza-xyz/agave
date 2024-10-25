use solana_define_syscall::define_syscall;

define_syscall!(fn sol_get_sysvar(sysvar_id_addr: *const u8, result: *mut u8, offset: u64, length: u64) -> u64);
