//! Define BLS certificate to be sent all to all in Alpenglow
use {
    serde::{Deserialize, Serialize},
    solana_hash::Hash,
    solana_program::clock::Slot,
};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
/// Certificate Type in Alpenglow
pub enum CertificateType {
    /// Finalize slow: at least 60 percent Finalize
    Finalize,
    /// Finalize fast: at least 80 percent Notarize
    FinalizeFast,
    /// Notarize: at least 60 percent Notarize
    Notarize,
    /// Notarize fallback: at least 60 percent Notarize or NotarizeFallback
    NotarizeFallback,
    /// Skip: at least 60 percent Skip or SkipFallback
    Skip,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// Certificate Type in Alpenglow
pub struct Certificate {
    /// Certificate type
    pub certificate_type: CertificateType,
    /// The slot of the block
    pub slot: Slot,
    /// The block id of the block
    pub block_id: Option<Hash>,
}
