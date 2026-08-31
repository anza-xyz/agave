//! Borrowed mirrors of the Alpenglow block footer types from `solana-entry`.
//!
//! These types mirror `solana_entry::block_component::VersionedBlockFooter`
//! and the certificate types it embeds, defined here so the plugin interface
//! does not depend on agave-internal crates. Variable-length data is exposed
//! as slices rather than owned `Vec`s: `Vec` is `#[repr(Rust)]` with no
//! stable layout, so only references to slices cross the plugin boundary.
//! The conversion happens at the agave boundary
//! (`solana-geyser-plugin-manager`) before a plugin is notified and performs
//! no allocation; if the internal types drift, that conversion fails to
//! compile rather than silently breaking plugins.

use {solana_bls_signatures::SignatureCompressed, solana_clock::Slot, solana_hash::Hash};

/// A compressed BLS aggregate signature plus the validator bitmap it covers.
///
/// Mirrors `solana_entry::block_component::VotesAggregate`. See
/// `solana-signer-store` for the bitmap encoding format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct VotesAggregate<'a> {
    /// The compressed BLS aggregate signature.
    pub signature: SignatureCompressed,
    /// The bitmap identifying the validators covered by the signature.
    pub bitmap: &'a [u8],
}

/// Certificate attesting the finalization of a block.
///
/// Mirrors `solana_entry::block_component::BlockFinalizationCert`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct BlockFinalizationCert<'a> {
    /// The slot the certificate is for.
    pub slot: Slot,
    /// The block id the certificate is for.
    pub block_id: Hash,
    /// The finalization vote aggregate.
    pub final_aggregate: VotesAggregate<'a>,
    /// The notarization vote aggregate, when present.
    pub notar_aggregate: Option<VotesAggregate<'a>>,
}

/// Reward certificate for the validators that voted skip.
///
/// Mirrors `agave_votor_messages::reward_certificate::SkipRewardCertificate`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SkipRewardCertificate<'a> {
    /// The slot the certificate is for.
    pub slot: Slot,
    /// The compressed BLS signature.
    pub signature: SignatureCompressed,
    /// The bitmap identifying the validators covered by the signature.
    pub bitmap: &'a [u8],
}

/// Reward certificate for the validators that voted notar.
///
/// Mirrors `agave_votor_messages::reward_certificate::NotarRewardCertificate`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NotarRewardCertificate<'a> {
    /// The slot the certificate is for.
    pub slot: Slot,
    /// The block id the certificate is for.
    pub block_id: Hash,
    /// The compressed BLS signature.
    pub signature: SignatureCompressed,
    /// The bitmap identifying the validators covered by the signature.
    pub bitmap: &'a [u8],
}

/// Block production metadata. User agent is capped at 255 bytes.
///
/// Mirrors `solana_entry::block_component::BlockFooterV1`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct BlockFooterV1<'a> {
    /// The bank hash of the block.
    pub bank_hash: Hash,
    /// Block production time in nanoseconds since the unix epoch.
    pub block_producer_time_nanos: u64,
    /// The user agent of the block producer.
    pub block_user_agent: &'a [u8],
    /// Certificate attesting the finalization of the block, when present.
    pub block_final_cert: Option<BlockFinalizationCert<'a>>,
    /// Skip reward certificate, when present.
    pub skip_reward_cert: Option<SkipRewardCertificate<'a>>,
    /// Notar reward certificate, when present.
    pub notar_reward_cert: Option<NotarRewardCertificate<'a>>,
}

/// A versioned Alpenglow block footer.
///
/// Mirrors `solana_entry::block_component::VersionedBlockFooter`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub enum VersionedBlockFooter<'a> {
    /// Version 1 of the block footer.
    V1(BlockFooterV1<'a>),
}
