#![no_std]
#![no_main]

use {
    crate::helpers::{
        ExtractedHeader, Reject, Verdict, check_gossip_fingerprint, check_quic_fingerprint,
        check_repair_fingerprint, check_turbine_size, read_at, vote::check_vote_signature,
    },
    agave_xdp_ebpf::{FIREWALL_RULES_LEN, FirewallConfig, FirewallRule, RejectEvent, RejectReason},
    aya_ebpf::{
        bindings::xdp_action::{XDP_DROP, XDP_PASS},
        helpers::generated::bpf_xdp_get_buff_len,
        macros::{map, xdp},
        maps::{Array, RingBuf},
        programs::XdpContext,
    },
    core::ptr,
};

mod helpers;

#[unsafe(no_mangle)]
// Set to 1 from user space at load time to control whether we must drop multi-frags packets
static AGAVE_XDP_DROP_MULTI_FRAGS: u8 = 0;

/// Global firewall settings, written by user space at load time.
#[map]
static FIREWALL_CONFIG: Array<FirewallConfig> = Array::with_max_entries(1, 0);

/// Firewall rules indexed by UDP destination port.
#[map]
static FIREWALL_RULES: Array<FirewallRule> = Array::with_max_entries(FIREWALL_RULES_LEN, 0);

/// Reports packets rejected by the firewall to user space.
#[map]
static FIREWALL_EVENTS: RingBuf = RingBuf::with_byte_size(4096 * 64, 0);

#[xdp]
pub fn agave_xdp(ctx: XdpContext) -> u32 {
    if drop_frags() && has_frags(&ctx) {
        // We're not actually dropping any valid frames here. See
        // https://lore.kernel.org/netdev/20251021173200.7908-2-alessandro.d@gmail.com
        return XDP_DROP;
    }
    let Some(config) = FIREWALL_CONFIG.get(0) else {
        return XDP_PASS;
    };
    // config not written by user space, firewall disabled
    if config.local_ip == [0; 4] {
        return XDP_PASS;
    }
    // Not IPv4 UDP or not parseable: let the kernel handle the packet normally
    let Ok(header) = ExtractedHeader::from_context(&ctx, config.strip_gre) else {
        return XDP_PASS;
    };
    let rule = FIREWALL_RULES
        .get(u32::from(header.dst_port))
        .copied()
        .unwrap_or_default();
    if rule == FirewallRule::Unmanaged {
        return XDP_PASS;
    }

    let Err(reject) = apply_firewall_rule(&ctx, &header, config, rule) else {
        return XDP_PASS;
    };
    report_reject(header.dst_port, reject);
    if config.enforce { XDP_DROP } else { XDP_PASS }
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

/// Non-protocol specific checks
#[inline(always)]
fn sanity_checks(header: &ExtractedHeader, config: &FirewallConfig) -> Verdict {
    // Solana services never run as root, so legit traffic never comes from privileged ports
    if header.src_port < 1024 {
        return Err(Reject::new(
            RejectReason::ReservedSourcePort,
            header.src_port,
        ));
    }
    if header.dst_ip != config.local_ip {
        return Err(Reject::new(RejectReason::WrongDestinationIp, 0));
    }
    Ok(())
}

/// Core protocol-specific logic of the firewall for Agave
#[inline(always)]
fn apply_firewall_rule(
    ctx: &XdpContext,
    header: &ExtractedHeader,
    config: &FirewallConfig,
    rule: FirewallRule,
) -> Verdict {
    sanity_checks(header, config)?;
    let first_byte = || read_at::<u8>(ctx, header.payload_offset);
    match rule {
        FirewallRule::Unmanaged | FirewallRule::Allow => Ok(()),
        FirewallRule::DenyIngress => Err(Reject::new(RejectReason::EgressOnlyPort, 0)),
        FirewallRule::Quic => check_quic_fingerprint(first_byte()),
        FirewallRule::Turbine => check_turbine_size(header),
        FirewallRule::Repair => check_repair_fingerprint(header, first_byte()),
        FirewallRule::Gossip => check_gossip_fingerprint(header, first_byte()),
        FirewallRule::Vote => check_vote_signature(header, first_byte()),
    }
}

#[inline(always)]
fn report_reject(dst_port: u16, reject: Reject) {
    let event = RejectEvent {
        dst_port,
        reason: reject.reason,
        reserved: 0,
        detail: reject.detail,
    };
    // ring buffer full: the event is lost, the packet is still handled
    let _ = FIREWALL_EVENTS.output::<RejectEvent>(&event, 0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // This is so that if we accidentally panic anywhere the verifier will refuse to load the
    // program as it'll detect an infinite loop.
    #[allow(clippy::empty_loop)]
    loop {}
}
