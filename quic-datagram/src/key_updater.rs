//! Identity rotation handle for [`crate::QuicDatagramEndpoint`].
//!
//! Construction surfaces an `Arc<KeyUpdater>` that implements
//! [`solana_tls_utils::NotifyKeyUpdate`] so callers can register it in the
//! validator's `KeyUpdaters` registry alongside the other rotatable
//! identity consumers. On `update_key`, a fresh `IdentitySnapshot` is
//! published on a `tokio::sync::watch` channel; the control loop applies
//! it asynchronously (rebuild TLS configs, swap them via quinn, evict the
//! connection table with `IDENTITY_ROTATED`, switch the lex-direction
//! pubkey).
//!
//! In-flight dials and queued egress at the time of rotation are not
//! retried - callers must re-send if they care. Steady-state vote
//! traffic recovers on the next slot's send.

use {
    rustls::pki_types::{CertificateDer, PrivateKeyDer},
    solana_keypair::{Keypair, Signer},
    solana_pubkey::Pubkey,
    solana_tls_utils::{NotifyKeyUpdate, new_dummy_x509_certificate},
    std::sync::Arc,
    tokio::sync::watch,
};

/// Identity material derived from a keypair: the ed25519 pubkey plus the
/// self-signed TLS cert/key that the endpoint presents to peers.
pub(crate) struct IdentitySnapshot {
    pub pubkey: Pubkey,
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl IdentitySnapshot {
    pub fn from_keypair(keypair: &Keypair) -> Self {
        let (cert, key) = new_dummy_x509_certificate(keypair);
        Self {
            pubkey: keypair.pubkey(),
            cert,
            key,
        }
    }
}

/// Handle for caller-driven identity rotation. Cloneable and thread-safe.
/// Implements [`NotifyKeyUpdate`] so it slots into the validator's existing
/// key-rotation plumbing.
pub struct KeyUpdater {
    tx: watch::Sender<Option<Arc<IdentitySnapshot>>>,
}

impl KeyUpdater {
    pub(crate) fn new() -> (Self, watch::Receiver<Option<Arc<IdentitySnapshot>>>) {
        let (tx, rx) = watch::channel(None);
        (Self { tx }, rx)
    }
}

impl NotifyKeyUpdate for KeyUpdater {
    fn update_key(&self, keypair: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let snap = Arc::new(IdentitySnapshot::from_keypair(keypair));
        self.tx
            .send(Some(snap))
            .map_err(|_| -> Box<dyn std::error::Error> {
                "quic-datagram endpoint has shut down; identity update rejected".into()
            })?;
        Ok(())
    }
}
