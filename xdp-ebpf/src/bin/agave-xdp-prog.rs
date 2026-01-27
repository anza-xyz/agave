#![no_std]
#![no_main]

use {
    crate::helpers::{
        check_gossip_fingerprint, check_quic_fingerprint, check_repair_fingerprint, get_or_default,
        vote::check_vote_signature, ExtractError,
    },
    agave_xdp_ebpf::{
        DecisionEvent, FirewallConfig, FirewallDecision, FirewallRule, MAX_FIREWALL_RULES,
    },
    aya_ebpf::{
        bindings::xdp_action::{XDP_DROP, XDP_PASS},
        helpers::gen::bpf_xdp_get_buff_len,
        macros::{map, xdp},
        maps::{Array, RingBuf},
        programs::XdpContext,
    },
    core::net::Ipv4Addr,
    helpers::ExtractedHeader,
};

pub mod helpers;

/// Ports on which to enact firewalling.
#[map]
static FIREWALL_CONFIG: Array<FirewallConfig> = Array::with_max_entries(1, 0);

#[map]
static FIREWALL_RULES: Array<FirewallRule> = Array::with_max_entries(MAX_FIREWALL_RULES as u32, 0);

/// Report the decision made by the firewall.
#[map]
static RING_BUF: RingBuf = RingBuf::with_byte_size(4096 * 64, 0);

#[xdp]
pub fn agave_xdp(ctx: XdpContext) -> u32 {
    // If we can not load config we just pass everything
    let Some(config) = FIREWALL_CONFIG.get(0) else {
        return XDP_PASS;
    };
    // Workaround for buggy drivers - not really part of the firewall
    if config.drop_frags && has_frags(&ctx) {
        // We're not actually dropping any valid frames here. See
        // https://lore.kernel.org/netdev/20251021173200.7908-2-alessandro.d@gmail.com
        return XDP_DROP;
    }
    // if configuration is invalid/incomplete, we pass everything
    if config.my_ip == Ipv4Addr::UNSPECIFIED {
        return XDP_PASS;
    }

    let header = match ExtractedHeader::from_context(&ctx, config.strip_gre) {
        Ok(header) => header,
        // TCP (and other things that are not UDP)
        Err(ExtractError::NotUdp) => return XDP_PASS,
        // IPv6, Arp
        Err(ExtractError::NotSupported) => return XDP_PASS,
        // encountered a packet we could not parse
        _ => {
            return XDP_PASS;
        }
    };

    if outside_valid_port_range(header.dst_port, config) {
        return XDP_PASS;
    }

    let decision = apply_xdp_firewall(&ctx, &header, config);
    if matches!(decision, FirewallDecision::Pass) {
        return XDP_PASS;
    }
    report_decision(header.dst_port, decision);
    if config.enforce {
        XDP_DROP
    } else {
        XDP_PASS
    }
}

#[inline(always)]
pub fn has_frags(ctx: &XdpContext) -> bool {
    #[allow(clippy::arithmetic_side_effects)]
    let linear_len = ctx.data_end() - ctx.data();
    // Safety: generated binding is unsafe, but static verifier guarantees ctx.ctx is valid.
    let buf_len = unsafe { bpf_xdp_get_buff_len(ctx.ctx) as usize };
    linear_len < buf_len
}

#[inline(always)]
fn outside_valid_port_range(dst_port: u16, config: &FirewallConfig) -> bool {
    dst_port < config.solana_min_port || dst_port > config.solana_min_port + MAX_FIREWALL_RULES
}

/// Non-protocol specific checks
fn sanity_checks(header: &ExtractedHeader, config: &FirewallConfig) -> Option<FirewallDecision> {
    // drop packets from "reserved" ports as Solana should never run as root
    if header.src_port < 1024 {
        return Some(FirewallDecision::ReservedPort);
    }
    // drop packets targeting IP which is not ours
    if header.dst_ip != config.my_ip {
        return Some(FirewallDecision::IpWrongDestination);
    }
    None
}

/// Core protocol-specific logic of the firewall for Agave
fn apply_xdp_firewall(
    ctx: &XdpContext,
    header: &ExtractedHeader,
    config: &FirewallConfig,
) -> FirewallDecision {
    if let Some(decision) = sanity_checks(&header, config) {
        return decision;
    }
    // find an offset into the rules array based on solana port range
    // if destination port is outside the range, the map lookup will fail
    let rule_offset = header.dst_port.saturating_sub(config.solana_min_port) as u32;
    // Default rule is DenyIngress, so if we can not find a rule for
    // a given port, the packet is dropped
    let rule = FIREWALL_RULES.get(rule_offset).cloned().unwrap_or_default();

    // this is to keep verifier happy (we are not setting first_byte on all variants of the match)
    #[allow(unused_assignments)]
    let mut first_byte = 0;
    match rule {
        FirewallRule::DenyIngress => FirewallDecision::TxOnlyPort(header.dst_port),
        FirewallRule::Quic => {
            first_byte = get_or_default(&ctx, header.payload_offset);
            check_quic_fingerprint(first_byte)
        }
        FirewallRule::Turbine => {
            // todo: check the shredversion byte
            if header.payload_len < 1200 {
                FirewallDecision::TurbineTooShort(header.payload_len as u16)
            } else {
                FirewallDecision::Pass
            }
        }
        FirewallRule::Repair => {
            first_byte = get_or_default(&ctx, header.payload_offset);
            check_repair_fingerprint(&header, first_byte)
        }
        FirewallRule::Gossip => {
            first_byte = get_or_default(&ctx, header.payload_offset);
            check_gossip_fingerprint(&header, first_byte)
        }
        FirewallRule::Vote => {
            first_byte = get_or_default(&ctx, header.payload_offset);
            check_vote_signature(&header, first_byte)
        }
    }
}

fn report_decision(dst_port: u16, decision: FirewallDecision) {
    let event = DecisionEvent { dst_port, decision };
    let _ = RING_BUF.output(&event, 0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // This is so that if we accidentally panic anywhere the verifier will refuse to load the
    // program as it'll detect an infinite loop.
    #[allow(clippy::empty_loop)]
    loop {}
}
