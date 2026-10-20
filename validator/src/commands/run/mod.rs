pub mod args;
pub mod execute;
mod storage;
#[cfg(target_os = "linux")]
mod xdp;

pub use {args::add_args, execute::execute};

pub struct Config {
    #[cfg(target_os = "linux")]
    pub primordial_caps: caps::CapsHashSet,
}
