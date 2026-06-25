pub mod args;
#[cfg(target_os = "linux")]
pub mod config_file;
pub mod execute;

pub use {args::add_args, execute::execute};

pub struct Config {
    #[cfg(target_os = "linux")]
    pub primordial_caps: caps::CapsHashSet,
}
