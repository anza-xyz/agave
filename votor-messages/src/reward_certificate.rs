//! Defines aggregates used for vote rewards.

use {
    crate::consensus_message::VoteMessage,
    solana_bls_signatures::SignatureCompressed as BLSSignatureCompressed,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_signer_store::EncodeError,
    thiserror::Error,
    wincode::{
        containers::{Pod, Vec as WincodeVec},
        len::ShortU16,
        SchemaRead, SchemaWrite,
    },
};

/// Number of slots in the past that the the current leader is responsible for producing the reward certificates.
pub const NUM_SLOTS_FOR_REWARD: u64 = 8;

/// Different types of errors that can be returned when constructing a new reward certificate.
#[derive(Debug, Error)]
pub enum RewardCertError {
    /// Invalid bitmap was supplied.
    #[error("invalid bitmap was supplied")]
    InvalidBitmap,
}

/// Reward certificate for the validators that voted skip.
///
/// Unlike the skip certificate which can be base-2 or base-3 encoded, this is guaranteed to be base-2 encoded.
#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct SkipRewardCertificate {
    /// The slot the certificate is for.
    pub slot: Slot,
    /// The signature.
    #[wincode(with = "Pod<BLSSignatureCompressed>")]
    pub signature: BLSSignatureCompressed,
    /// The bitmap for validators, see solana-signer-store for encoding format.
    #[wincode(with = "WincodeVec<u8, ShortU16>")]
    bitmap: Vec<u8>,
}

impl SkipRewardCertificate {
    /// Returns a new instance of [`SkipRewardCertificate`].
    pub fn try_new(
        slot: Slot,
        signature: BLSSignatureCompressed,
        bitmap: Vec<u8>,
    ) -> Result<Self, RewardCertError> {
        if bitmap.len() > u16::MAX as usize {
            return Err(RewardCertError::InvalidBitmap);
        }
        Ok(Self {
            slot,
            signature,
            bitmap,
        })
    }

    /// Returns a reference to the bitmap.
    pub fn to_bitmap(&self) -> &[u8] {
        &self.bitmap
    }

    /// Returns the bitmap consuming self.
    pub fn into_bitmap(self) -> Vec<u8> {
        self.bitmap
    }
}

/// Reward certificate for the validators that voted notar.
#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct NotarRewardCertificate {
    /// The slot the certificate is for.
    pub slot: Slot,
    /// The block id the certificate is for.
    pub block_id: Hash,
    /// The signature.
    #[wincode(with = "Pod<BLSSignatureCompressed>")]
    pub signature: BLSSignatureCompressed,
    /// The bitmap for validators, see solana-signer-store for encoding format.
    #[wincode(with = "WincodeVec<u8, ShortU16>")]
    bitmap: Vec<u8>,
}

impl NotarRewardCertificate {
    /// Returns a new instance of [`NotarRewardCertificate`].
    pub fn try_new(
        slot: Slot,
        block_id: Hash,
        signature: BLSSignatureCompressed,
        bitmap: Vec<u8>,
    ) -> Result<Self, RewardCertError> {
        if bitmap.len() > u16::MAX as usize {
            return Err(RewardCertError::InvalidBitmap);
        }
        Ok(Self {
            slot,
            block_id,
            signature,
            bitmap,
        })
    }

    /// Returns a reference to the bitmap.
    pub fn bitmap(&self) -> &[u8] {
        &self.bitmap
    }

    /// Returns the bitmap consuming self.
    pub fn into_bitmap(self) -> Vec<u8> {
        self.bitmap
    }
}

/// Message to add votes to the rewards container.
#[derive(Debug)]
pub struct AddVoteMessage {
    /// List of [`VoteMessage`]s.
    pub votes: Vec<VoteMessage>,
}

/// Request to build reward certificates.
pub struct BuildRewardCertsRequest {
    /// The bank slot which will include the built reward certs.
    pub bank_slot: Slot,
}

/// Response when the reward certs are built successfully.
#[derive(Default)]
pub struct BuildRewardCertsRespSucc {
    /// Skip reward certificate.  None if no skip votes were registered.
    pub skip: Option<SkipRewardCertificate>,
    /// Notar reward certificate.  None if no notar votes were registered.
    pub notar: Option<NotarRewardCertificate>,
    /// If at least one of the certs above is present, then this contains the slot for which the reward certs were built and the list of validators in the certs.
    pub validators: Vec<Pubkey>,
}

/// Error returned when build reward certs fails.
#[derive(Debug, Error)]
pub enum BuildRewardCertsRespError {
    /// Building either the skip or the notar reward cert failed.
    #[error("try_new() on skip or notar cert failed with {0}")]
    RewardCertTryNew(#[from] RewardCertError),
    /// Experienced failure with encoding.
    #[error("encode error {0:?}")]
    Encode(EncodeError),
}

/// A type alias to minimise making changes if the above types change.
pub type BuildRewardCertsResponse = Result<BuildRewardCertsRespSucc, BuildRewardCertsRespError>;
