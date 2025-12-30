use {agave_xdp::device::NetworkDevice, std::error::Error};

#[derive(Clone, Debug)]
pub struct FirewallConfig {
    pub network_device: NetworkDevice,
    pub zero_copy: bool,
}
#[cfg(not(target_os = "linux"))]
pub struct XdpFirewall {}
#[cfg(target_os = "linux")]
pub struct XdpFirewall {
    _ebpf: aya::Ebpf,
}

impl XdpFirewall {
    #[cfg(not(target_os = "linux"))]
    pub fn new(_config: FirewallConfig) -> Result<Self, Box<dyn Error>> {
        Err("XDP firewalling is only supported on Linux".into())
    }
    #[cfg(target_os = "linux")]
    pub fn new(config: FirewallConfig) -> Result<Self, Box<dyn Error>> {
        use caps::{
            CapSet,
            Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON},
        };

        let dev = config.network_device;

        // switch to higher caps while we setup XDP. We assume that an error in
        // this function is irrecoverable so we don't try to drop on errors.
        for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
            caps::raise(None, CapSet::Effective, cap)
                .map_err(|e| format!("failed to raise {cap:?} capability: {e}"))?;
        }
        let ebpf = agave_xdp::load_xdp_program(&dev)
            .map_err(|e| format!("failed to attach xdp program: {e}"))?;
        let firewall = XdpFirewall { _ebpf: ebpf };
        Ok(firewall)
    }
}
