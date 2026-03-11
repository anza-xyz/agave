#![cfg(feature = "agave-unstable-api")]
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "linux")]
pub mod device;
#[cfg(target_os = "linux")]
pub mod gre;
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

pub mod xdp_retransmitter;

#[cfg(target_os = "linux")]
pub use program::load_xdp_program;
use std::{io, net::Ipv4Addr};

#[cfg(target_os = "linux")]
pub fn set_cpu_affinity(cpus: impl IntoIterator<Item = usize>) -> Result<(), io::Error> {
    unsafe {
        let mut cpu_set = std::mem::zeroed();

        for cpu in cpus {
            libc::CPU_SET(cpu, &mut cpu_set);
        }

        let result = libc::sched_setaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            &cpu_set as *const libc::cpu_set_t,
        );
        if result != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_cpu_affinity(_cpus: impl IntoIterator<Item = usize>) -> Result<(), io::Error> {
    unimplemented!()
}

#[cfg(target_os = "linux")]
pub fn get_cpu() -> Result<usize, io::Error> {
    unsafe {
        let result = libc::sched_getcpu();
        if result < 0 {
            assert_eq!(result, -1);
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn get_cpu() -> Result<usize, io::Error> {
    unimplemented!()
}

#[cfg(target_os = "linux")]
/// Get the IPv4 address of the device
pub fn get_src_device_ip(maybe_interface: Option<String>) -> Ipv4Addr {
    let src_device = if let Some(interface) = maybe_interface {
        if let Some(ip) = master_ip_if_bonded(&interface) {
            return ip;
        } else {
            crate::device::NetworkDevice::new(interface).unwrap()
        }
    } else {
        crate::device::NetworkDevice::new_from_default_route().unwrap()
    };
    src_device
        .ipv4_addr()
        .expect("no src_ip provided, device must have an IPv4 address")
}

#[cfg(not(target_os = "linux"))]
pub fn get_src_device_ip(_maybe_interface: Option<String>) -> Ipv4Addr {
    unimplemented!()
}

/// Returns the IPv4 address of the master interface if the given interface is part of a bond.
#[cfg(target_os = "linux")]
fn master_ip_if_bonded(interface: &str) -> Option<Ipv4Addr> {
    let master_ifindex_path = format!("/sys/class/net/{interface}/master/ifindex");
    if let Ok(contents) = std::fs::read_to_string(&master_ifindex_path) {
        let idx = contents.trim().parse().unwrap();
        return Some(
            crate::device::NetworkDevice::new_from_index(idx)
                .and_then(|dev| dev.ipv4_addr())
                .unwrap_or_else(|e| {
                    panic!(
                        "failed to open bond master interface for {interface}: master index \
                         {idx}: {e}"
                    )
                }),
        );
    }
    None
}
