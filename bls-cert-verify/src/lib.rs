#![cfg(feature = "agave-unstable-api")]
//! This crate implements the certificate verification logic for Alpenglow.
//!
//! This logic is shared across multiple components, including:
//! - The BLS Sigverifier in `solana-core`
//! - The verification for certs in block markers in `solana-runtime`
//!
//! The main entry point for this crate is the [`cert_verify::verify_certificate`]
//! function.

pub mod cert_verify;
