//! Functionality for public and private keys.
#![cfg(feature = "full")]

// legacy module paths
#[deprecated(
    since = "2.2.0",
    note = "Use solana_keypair::signable::Signable instead."
)]
pub use solana_keypair::signable::Signable;
pub use {
    crate::signer::{keypair::*, null_signer::*, presigner::*, *},
    solana_signature::{ParseSignatureError, Signature, SIGNATURE_BYTES},
};
