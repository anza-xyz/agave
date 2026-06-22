pub mod args;
pub mod execute;
#[cfg(target_os = "linux")]
pub mod xdp_config_file;

pub use {args::add_args, execute::execute};

pub struct Config {
    #[cfg(target_os = "linux")]
    pub primordial_caps: caps::CapsHashSet,
}
