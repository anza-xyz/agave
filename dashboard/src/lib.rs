#![cfg(feature = "agave-unstable-api")]
//! A web dashboard for the Agave validator: a single-page app and a websocket
//! feed of its state on one port, read through handles the validator already
//! holds.

pub mod collect;
pub mod config;
pub mod context;
/// Shared by the tests in several modules, so it lives at the crate root
/// rather than being rebuilt inside each `mod tests`.
#[cfg(test)]
pub(crate) mod fixture;
pub mod history;
pub mod host_stats;
pub mod meters;
pub mod metrics_tap;
pub mod net_stats;
pub mod produced;
pub mod proto;
pub mod server;
pub mod service;
pub mod slots;
pub mod startup;
pub mod thread_stats;
pub mod tips;
pub mod udp_drops;
pub mod validator_info;

pub use {config::DashboardConfig, context::DashboardContext, service::DashboardService};
