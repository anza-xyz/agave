use {
    crate::Certificate,
    bitvec::prelude::*,
    solana_bls_signatures::{BlsError, Pubkey as BlsPubkey, PubkeyProjective, SignatureProjective},
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    solana_votor_messages::bls_message::{CertificateMessage, VoteMessage},
    thiserror::Error,
};

/// Maximum number of validators in a certificate
///
/// There are around 1500 validators currently. For a clean power-of-two
/// implementation, we should chosoe either 2048 or 4096. Choose a more
/// conservative number 4096 for now.
const MAXIMUM_VALIDATORS: usize = 4096;

#[derive(Debug, Error, PartialEq)]
pub enum CertificateError {
    #[error("BLS error: {0}")]
    BlsError(#[from] BlsError),
    #[error("Invalid pubkey")]
    InvalidPubkey,
    #[error("Validator does not exist for given rank: {0}")]
    ValidatorDoesNotExist(usize),
}

/// A builder for creating a `CertificateMessage` by efficiently aggregating BLS signatures.
#[derive(Clone)]
pub struct VoteCertificateBuilder {
    certificate: Certificate,
    signature: SignatureProjective,
    bitmap: BitVec<u8, Lsb0>,
}

impl TryFrom<CertificateMessage> for VoteCertificateBuilder {
    type Error = CertificateError;

    fn try_from(message: CertificateMessage) -> Result<Self, Self::Error> {
        let projective_signature = SignatureProjective::try_from(message.signature)?;
        Ok(VoteCertificateBuilder {
            certificate: message.certificate,
            signature: projective_signature,
            bitmap: message.bitmap,
        })
    }
}

impl VoteCertificateBuilder {
    pub fn new(certificate_id: Certificate) -> Self {
        Self {
            certificate: certificate_id,
            signature: SignatureProjective::identity(),
            bitmap: BitVec::<u8, Lsb0>::repeat(false, MAXIMUM_VALIDATORS),
        }
    }

    /// Aggregates a slice of `VoteMessage`s into the builder.
    pub fn aggregate(&mut self, messages: &[VoteMessage]) -> Result<(), CertificateError> {
        for vote_message in messages {
            if self.bitmap.len() <= vote_message.rank as usize {
                return Err(CertificateError::ValidatorDoesNotExist(
                    vote_message.rank as usize,
                ));
            }
            self.bitmap.set(vote_message.rank as usize, true);
        }
        let signature_iter = messages.iter().map(|vote_message| &vote_message.signature);
        Ok(self.signature.aggregate_with(signature_iter)?)
    }

    pub fn build(self) -> CertificateMessage {
        CertificateMessage {
            certificate: self.certificate,
            signature: self.signature.into(),
            bitmap: self.bitmap,
        }
    }
}

/// Given a bit vector and a list of validator BLS pubkeys, generate an
/// aggregate BLS pubkey.
#[allow(dead_code)]
pub fn aggregate_pubkey(
    bitmap: &BitVec<u8, Lsb0>,
    bls_pubkey_to_rank_map: &BLSPubkeyToRankMap,
) -> Result<BlsPubkey, CertificateError> {
    let mut aggregate_pubkey = PubkeyProjective::identity();
    for (i, included) in bitmap.iter().enumerate() {
        if *included {
            let bls_pubkey: PubkeyProjective = bls_pubkey_to_rank_map
                .get_pubkey(i)
                .ok_or(CertificateError::ValidatorDoesNotExist(i))?
                .1
                .try_into()
                .map_err(|_| CertificateError::InvalidPubkey)?;

            aggregate_pubkey.aggregate_with([&bls_pubkey])?;
        }
    }

    Ok(aggregate_pubkey.into())
}
