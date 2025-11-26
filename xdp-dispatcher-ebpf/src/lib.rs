#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(missing_unsafe_on_extern)]
#![warn(rust_2024_guarded_string_incompatible_syntax)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]
#![no_std]

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct XdpDispatcherConfig {
    pub num_progs_enabled: u8,
    pub chain_call_actions: [u32; 10],
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
unsafe impl aya::Pod for XdpDispatcherConfig {}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
pub static AGAVE_XDP_DISPATCHER_EBPF_PROGRAM: &[u8] = aya::include_bytes_aligned!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agave-xdp-dispatcher-prog"
));

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
#[unsafe(no_mangle)]
pub static AGAVE_XDP_DISPATCHER_EBPF_PROGRAM_BYTES: [u8; AGAVE_XDP_DISPATCHER_EBPF_PROGRAM.len()] =
    unsafe { core::ptr::read(AGAVE_XDP_DISPATCHER_EBPF_PROGRAM.as_ptr().cast()) };
