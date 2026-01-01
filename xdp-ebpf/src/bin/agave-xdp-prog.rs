#![no_std]
#![no_main]

use {
    crate::helpers::{get_or_default, has_quic_fixed_bit, ExtractError},
    agave_xdp_ebpf::{DecisionEvent, FirewallConfig, FirewallDecision},
    aya_ebpf::{
        bindings::xdp_action::{XDP_DROP, XDP_PASS},
        helpers::gen::bpf_xdp_get_buff_len,
        macros::{map, xdp},
        maps::{Array, RingBuf},
        programs::XdpContext,
    },
    aya_log_ebpf::error,
    core::net::Ipv4Addr,
    helpers::ExtractedHeader,
};

mod helpers;

/// Ports on which to enact firewalling.
#[map]
static FIREWALL_CONFIG: Array<FirewallConfig> = Array::with_max_entries(1, 0);

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
            error!(&ctx, "FIREWALL could not parse packet");
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
    dst_port < config.solana_min_port || dst_port > config.solana_max_port
}

/// Core protocol-specific logic of the firewall for Agave
fn apply_xdp_firewall(
    ctx: &XdpContext,
    header: &ExtractedHeader,
    config: &FirewallConfig,
) -> FirewallDecision {
    // drop things from "reserved" ports
    if header.src_port < 1024 {
        return FirewallDecision::ReservedPort;
    }
    if header.dst_ip != config.my_ip {
        return FirewallDecision::IpWrongDestination;
    }

    let first_byte: u8 = get_or_default(&ctx, header.payload_offset);
    if header.dst_port == config.tpu_vote {
        if header.payload_len < 64 {
            return FirewallDecision::VoteTooShort;
        }
    }
    if header.dst_port == config.gossip {
        if header.payload_len < 132 {
            return FirewallDecision::GossipTooShort;
        }
        if first_byte > 5 {
            return FirewallDecision::GossipFingerprint;
        }
    }
    if header.dst_port == config.turbine {
        // turbine port receives shreds
        if header.payload_len < 1200 {
            return FirewallDecision::TurbineTooShort(header.payload_len as u16);
        }
    }
    if config
        .deny_ingress_ports
        .iter()
        .copied()
        .find(|&port| port == header.dst_port)
        .is_some()
    {
        // some ports are only used to send data
        return FirewallDecision::TxOnlyPort(header.dst_port);
    }
    if (header.dst_port == config.tpu_quic)
        || (header.dst_port == config.tpu_vote_quic)
        || (header.dst_port == config.tpu_forwards_quic)
    {
        // these ports receive via QUIC
        if !has_quic_fixed_bit(first_byte) {
            return FirewallDecision::NotQuicPacket;
        }
    }
    if header.dst_port == config.repair {
        if header.payload_len < 132 {
            return FirewallDecision::RepairTooShort(header.payload_len as u16);
        }
        if (first_byte < 6) || (first_byte > 11) {
            return FirewallDecision::RepairFingerprint(first_byte);
        }
    }
    if header.dst_port == config.serve_repair {
        if header.payload_len < 132 {
            return FirewallDecision::ServeRepairTooShort(header.payload_len as u16);
        }
        if (first_byte < 6) || (first_byte > 11) {
            return FirewallDecision::RepairFingerprint(first_byte);
        }
    }
    if header.dst_port == config.ancestor_repair {
        if header.payload_len < 132 {
            return FirewallDecision::AncestorRepairTooShort(header.payload_len as u16);
        }
        if (first_byte < 6) || (first_byte > 11) {
            return FirewallDecision::RepairFingerprint(first_byte);
        }
    }
    FirewallDecision::Pass
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
