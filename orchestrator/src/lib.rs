#[cfg(unix)]
mod config;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use config::*;
#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
pub type OrchestratorStream = std::os::unix::net::UnixStream;
#[cfg(not(unix))]
pub type OrchestratorStream = ();
