pub mod args;
// Parsing, merging and validation run on every platform; the host-resolution half
// is unused off Linux, where XDP transmit does not exist.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod config_file;
pub mod execute;

pub use {args::add_args, execute::execute};

pub struct Config {
    #[cfg(target_os = "linux")]
    pub primordial_caps: caps::CapsHashSet,
}
