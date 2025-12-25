#![no_std]
#![no_main]

use {
    crate::helpers::ExtractError,
    agave_xdp_ebpf::Flags,
    aya_ebpf::{
        bindings::xdp_action::{XDP_ABORTED, XDP_DROP, XDP_PASS},
        macros::{map, xdp},
        maps::{Array, BloomFilter, HashMap},
        programs::XdpContext,
    },
    aya_log_ebpf::info,
    core::{net::Ipv4Addr, ptr},
    helpers::{array_has_entry, has_frags, ExtractedHeader},
    network_types::{
        eth::{EthHdr, EtherType},
        ip::Ipv4Hdr,
    },
};

mod helpers;

/// Set to 1 from user space at load time to control whether we must drop multi-frags packets
#[unsafe(no_mangle)]
static AGAVE_XDP_DROP_MULTI_FRAGS: u8 = 0;

/// IPs of top 4096 staked nodes
#[map]
static mut STAKED_NODE_IPS: BloomFilter<u32> = BloomFilter::with_max_entries(1024 * 4, 0);

/// Ports on which only traffic from staked nodes is allowed
#[map]
static ALLOW_DST_PORTS_STAKED_ONLY: Array<u16> = Array::with_max_entries(16, 0);

#[xdp]
pub fn agave_xdp(ctx: XdpContext) -> u32 {
    if drop_frags() && has_frags(&ctx) {
        // We're not actually dropping any valid frames here. See
        // https://lore.kernel.org/netdev/20251021173200.7908-2-alessandro.d@gmail.com
        return XDP_DROP;
    }
    match try_xdp_firewall(ctx) {
        Ok(ret) => ret,
        Err(_) => XDP_ABORTED,
    }
}

#[inline]
fn drop_frags() -> bool {
    // SAFETY: This variable is only ever modified at load time, we need the volatile read to
    // prevent the compiler from optimizing it away.
    unsafe { ptr::read_volatile(&AGAVE_XDP_DROP_MULTI_FRAGS) == 1 }
}

#[allow(static_mut_refs)]
fn is_staked_ip(src_ip: Ipv4Addr) -> bool {
    unsafe { STAKED_NODE_IPS.contains(&src_ip.to_bits()).is_ok() }
}

fn try_xdp_firewall(ctx: XdpContext) -> Result<u32, ()> {
    let action;
    let header = match ExtractedHeader::from_context(&ctx, Flags::default()) {
        Ok(header) => header,
        Err(ExtractError::Ipv6 | ExtractError::NotUdp) => return Ok(XDP_PASS),
        // encountered a packet we could not parse
        _ => return Err(()),
    };

    if array_has_entry(&ALLOW_DST_PORTS_STAKED_ONLY, 16, header.dst_port) {
        action = if is_staked_ip(header.src_ip) {
            XDP_PASS
        } else {
            XDP_DROP
        };
    } else {
        action = XDP_PASS;
    }

    info!(
        &ctx,
        "SRC: {}:{}, DST PORT:{}, ACTION: {}",
        header.src_ip,
        header.src_port,
        header.dst_port,
        action
    );

    Ok(action)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // This is so that if we accidentally panic anywhere the verifier will refuse to load the
    // program as it'll detect an infinite loop.
    #[allow(clippy::empty_loop)]
    loop {}
}
