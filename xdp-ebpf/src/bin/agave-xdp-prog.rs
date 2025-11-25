#![no_std]
#![no_main]

use {
    aya_ebpf::{
        bindings::xdp_action::{XDP_DROP, XDP_PASS},
        helpers::gen::bpf_xdp_get_buff_len,
        macros::xdp,
        programs::XdpContext,
    },
    core::ptr,
};

#[no_mangle]
// Set to 1 from user space at load time to control whether we must drop multi-frags packets
static AGAVE_XDP_DROP_MULTI_FRAGS: u8 = 0;

#[xdp]
pub fn agave_xdp(ctx: XdpContext) -> u32 {
    if drop_frags() && has_frags(&ctx) {
        // We're not actually dropping any valid frames here. See
        // https://lore.kernel.org/netdev/20251021173200.7908-2-alessandro.d@gmail.com
        XDP_DROP
    } else {
        // let the kernel handle the packet normally
        XDP_PASS
    }
}

#[inline]
fn drop_frags() -> bool {
    // SAFETY: This variable is only ever modified at load time, we need the volatile read to
    // prevent the compiler from optimizing it away.
    unsafe { ptr::read_volatile(&AGAVE_XDP_DROP_MULTI_FRAGS) == 1 }
}

#[inline]
fn has_frags(ctx: &XdpContext) -> bool {
    #[allow(clippy::arithmetic_side_effects)]
    let linear_len = ctx.data_end() - ctx.data();
    // Safety: generated binding is unsafe, but static verifier guarantees ctx.ctx is valid.
    let buf_len = unsafe { bpf_xdp_get_buff_len(ctx.ctx) as usize };
    linear_len < buf_len
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // This is so that if we accidentally panic anywhere the verifier will refuse to load the
    // program as it'll detect an infinite loop.
    #[allow(clippy::empty_loop)]
    loop {}
}
