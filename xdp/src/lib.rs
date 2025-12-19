#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]

#[cfg(target_os = "linux")]
pub mod device;
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

#[cfg(target_os = "linux")]
pub use program::load_xdp_program;
use std::io;

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
