#[cfg(target_os = "linux")]
pub use crate::device::NetworkDevice;
use std::error::Error;

#[cfg(target_os = "linux")]
pub fn get_network_device(interface: Option<&str>) -> Result<NetworkDevice, Box<dyn Error>> {
    // switch to higher caps while we setup XDP. We assume that an error in
    // this function is irrecoverable so we don't try to drop on errors.
    use caps::{
        CapSet,
        Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON},
    };
    for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
        caps::raise(None, CapSet::Effective, cap)
            .map_err(|e| format!("failed to raise {cap:?} capability: {e}"))?;
    }

    let dev = if let Some(interface) = interface {
        NetworkDevice::new(interface)?
    } else {
        NetworkDevice::new_from_default_route()?
    };

    for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
        caps::drop(None, CapSet::Effective, cap)?;
    }
    Ok(dev)
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone)]
pub struct NetworkDevice {
    pub if_name: String,
}

#[cfg(not(target_os = "linux"))]
pub fn get_network_device(_interface: Option<String>) -> Result<NetworkDevice, Box<dyn Error>> {
    Err("XDP not supported on this platform!".into())
}
