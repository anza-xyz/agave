#![cfg(feature = "agave-unstable-api")]
//! Collection of TLS related code fragments that end up popping up everywhere where quic is used.
//! Aggregated here to avoid bugs due to conflicting implementations of the same functionality.

mod config;
pub use config::*;

mod crypto_provider;
pub use crypto_provider::*;

mod tls_certificates;
pub use tls_certificates::*;

mod quic_client_certificate;
pub use quic_client_certificate::*;

mod skip_server_verification;
pub use skip_server_verification::SkipServerVerification;

mod skip_client_verification;
pub use skip_client_verification::SkipClientVerification;

pub mod notify_key_update;
pub use notify_key_update::NotifyKeyUpdate;
