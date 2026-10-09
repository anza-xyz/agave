#![cfg(feature = "agave-unstable-api")]

#[cfg(target_os = "linux")]
pub mod device;
#[cfg(target_os = "linux")]
pub mod gre;
#[cfg(target_os = "linux")]
pub(crate) mod lpm;
#[cfg(target_os = "linux")]
pub(crate) mod neighbors;
#[cfg(target_os = "linux")]
pub mod netlink;
#[cfg(target_os = "linux")]
pub mod packet;
#[cfg(target_os = "linux")]
mod program;
#[cfg(target_os = "linux")]
pub mod route;
#[cfg(target_os = "linux")]
pub mod route_monitor;
#[cfg(target_os = "linux")]
pub mod socket;
#[cfg(target_os = "linux")]
pub mod tx_loop;
#[cfg(target_os = "linux")]
pub mod umem;

pub mod ecn_codepoint;

pub mod transmitter;

#[cfg(target_os = "linux")]
pub use program::{LoadXdpProgramError, load_xdp_program};
use std::{io, net::Ipv4Addr, num::ParseIntError};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InterfaceIpError {
    #[error("invalid bond master index for {interface}: {source}")]
    InvalidBondMasterIndex {
        interface: String,
        source: ParseIntError,
    },
    #[error("failed to resolve bond master IPv4 for {interface} (index {index}): {source}")]
    ResolveBondMasterIp {
        interface: String,
        index: u32,
        source: io::Error,
    },
    #[error("failed to resolve IPv4 address of {interface}: {source}")]
    ResolveInterfaceIp {
        interface: String,
        source: io::Error,
    },
}

/// Returns the IPv4 address of the specified network interface.
///
/// If the interface is part of a bonded interface, returns the master's IPv4 address.
#[cfg(target_os = "linux")]
pub fn interface_ipv4(interface: &str) -> Result<Ipv4Addr, InterfaceIpError> {
    if let Some(ip) = crate::transmitter::read_bond_master_ip(interface)? {
        Ok(ip)
    } else {
        crate::device::NetworkDevice::new(interface)
            .and_then(|device| device.ipv4_addr())
            .map_err(|source| InterfaceIpError::ResolveInterfaceIp {
                interface: interface.to_string(),
                source,
            })
    }
}

#[cfg(not(target_os = "linux"))]
pub fn interface_ipv4(_interface: &str) -> Result<Ipv4Addr, InterfaceIpError> {
    unimplemented!()
}

/// Returns the IPv4 address of the device associated with the default route.
#[cfg(target_os = "linux")]
pub fn default_device_ipv4() -> Result<Ipv4Addr, io::Error> {
    crate::device::NetworkDevice::new_from_default_route()
        .map_err(io::Error::other)?
        .ipv4_addr()
}

#[cfg(not(target_os = "linux"))]
pub fn default_device_ipv4() -> Result<Ipv4Addr, io::Error> {
    unimplemented!()
}
