#[cfg(unix)]
pub mod client;
#[cfg(windows)]
#[path = "client_windows.rs"]
pub mod client;
#[cfg(unix)]
pub mod server;
#[cfg(windows)]
#[path = "server_windows.rs"]
pub mod server;
mod shared;
#[cfg(test)]
mod tests;

pub use shared::*;
