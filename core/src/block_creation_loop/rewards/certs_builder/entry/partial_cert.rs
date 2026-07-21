use {
    super::AddVoteError,
    agave_bls_sigverify::rewards::RewardVoteMessage,
    bitvec::{order::Lsb0, vec::BitVec},
    solana_bls_signatures::{
        Signature as BLSSignature, SignatureCompressed as BLSSignatureCompressed,
        SignatureProjective,
    },
    solana_pubkey::Pubkey,
    solana_signer_store::{EncodeError, encode_base2},
    thiserror::Error,
};

/// Different types of errors that can be returned from building signature and the associated bitmap.
#[derive(Debug, Error)]
pub(super) enum BuildSigBitmapError {
    #[error("Encoding failed: {0:?}")]
    Encode(EncodeError),
    #[error("Empty bitvec")]
    Empty,
}

/// Struct to hold state for building a single reward cert.
#[derive(Clone)]
pub(super) struct PartialCert {
    /// In progress signature aggregate.
    signature: SignatureProjective,
    /// bitvec of ranks whose signatures is included in the aggregate above.
    bitvec: BitVec<u8, Lsb0>,
    /// total stake represented by the signatures in the aggregate above.
    stake: u64,
    validators: Vec<Pubkey>,
}

impl PartialCert {
    /// Returns a new instance of [`PartialCert`].
    pub(super) fn new(max_validators: usize) -> Self {
        Self {
            signature: SignatureProjective::identity(),
            bitvec: BitVec::repeat(false, max_validators),
            stake: 0,
            validators: Vec::with_capacity(max_validators),
        }
    }

    /// Returns true if the [`PartialCert`] needs the vote else false.
    pub(super) fn wants_vote(&self, rank: u16) -> bool {
        match self.bitvec.get(rank as usize) {
            None => false,
            Some(ind) => !*ind,
        }
    }

    /// Adds a new observed vote to the aggregate.
    pub(super) fn add_vote(&mut self, msg: &RewardVoteMessage) -> Result<(), AddVoteError> {
        match self.bitvec.get_mut(msg.rank as usize) {
            None => return Err(AddVoteError::InvalidRank),
            Some(mut ind) => {
                if *ind {
                    return Err(AddVoteError::Duplicate);
                }
                self.signature
                    .aggregate_with(std::iter::once(&msg.signature))?;
                self.validators.push(msg.vote_account_pubkey);
                self.stake = self.stake.saturating_add(msg.stake.get());
                *ind = true;
            }
        }
        Ok(())
    }

    /// Builds a signature and associated bitmap from the collected votes.
    ///
    /// On success, returns the built signature, bitmap, and the list of validators in the bitmap.
    pub(super) fn build_sig_bitmap(
        self,
    ) -> Result<(BLSSignatureCompressed, Vec<u8>, Vec<Pubkey>), BuildSigBitmapError> {
        if self.validators.is_empty() {
            return Err(BuildSigBitmapError::Empty);
        }
        let mut bitvec = self.bitvec.clone();
        let new_len = bitvec.last_one().map_or(0, |i| i.saturating_add(1));
        bitvec.resize(new_len, false);
        let bitmap = encode_base2(&bitvec).map_err(BuildSigBitmapError::Encode)?;
        let signature = BLSSignature::from(self.signature).try_into().unwrap();
        Ok((signature, bitmap, self.validators))
    }

    /// Returns how much stake has been observed.
    pub(super) fn stake(&self) -> u64 {
        self.stake
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::block_creation_loop::rewards::certs_builder::entry::tests::{
            get_keypairs, new_reward_vote_msg, validate_bitmap,
        },
        agave_bls_sigverify::rewards::RewardVote,
        agave_votor_messages::{
            consensus_message::VoteMessage,
            vote::{SkipVote, Vote},
            wire::get_vote_payload_to_sign,
        },
        rand::Rng,
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Keypair as BlsKeypair},
        std::num::NonZero,
    };

    fn new_invalid_vote(vote: Vote, rank: usize) -> VoteMessage {
        let serialized = get_vote_payload_to_sign(vote, 0);
        let keypair = BlsKeypair::new();
        let signature = keypair.sign(&serialized).into();
        VoteMessage {
            vote,
            signature,
            rank: rank.try_into().unwrap(),
        }
    }

    #[test]
    fn validate_build_sig_bitmap() {
        let slot = 123;
        let max_validators = 2;
        let shred_version = rand::rng().random();
        let keypairs = get_keypairs(max_validators, slot);
        let mut partial_cert = PartialCert::new(max_validators);
        assert!(matches!(
            partial_cert.clone().build_sig_bitmap(),
            Err(BuildSigBitmapError::Empty)
        ));
        let skip = Vote::new_skip_vote(slot);
        for rank in 0..max_validators {
            let reward_vote_msg = new_reward_vote_msg(skip, rank, &keypairs, None, shred_version);
            partial_cert.add_vote(&reward_vote_msg).unwrap();
            let (_signature, bitmap, _) = partial_cert.clone().build_sig_bitmap().unwrap();
            validate_bitmap(&bitmap, rank + 1, max_validators);
        }
    }

    #[test]
    fn validate_add_vote() {
        let slot = 123;
        let max_validators = 2;
        let shred_version = rand::rng().random();
        let keypairs = get_keypairs(max_validators, slot);
        let mut partial_cert = PartialCert::new(max_validators);
        let reward_vote = RewardVote::Skip(SkipVote { slot });
        let invalid_reward_vote_msg = RewardVoteMessage {
            vote: reward_vote,
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            rank: 2,
            stake: NonZero::new(1234).unwrap(),
            vote_account_pubkey: Pubkey::new_unique(),
        };
        let skip = Vote::new_skip_vote(slot);
        assert!(matches!(
            partial_cert.add_vote(&invalid_reward_vote_msg),
            Err(AddVoteError::InvalidRank)
        ));
        let reward_vote_msg = new_reward_vote_msg(skip, 0, &keypairs, None, shred_version);
        partial_cert.add_vote(&reward_vote_msg).unwrap();
        assert!(matches!(
            partial_cert.add_vote(&reward_vote_msg),
            Err(AddVoteError::Duplicate)
        ));
        let reward_vote_msg = new_reward_vote_msg(skip, 1, &keypairs, None, shred_version);
        partial_cert.add_vote(&reward_vote_msg).unwrap();
        let reward_vote_msg = new_reward_vote_msg(skip, 0, &keypairs, None, shred_version);
        assert!(matches!(
            partial_cert.add_vote(&reward_vote_msg),
            Err(AddVoteError::Duplicate)
        ));
    }

    #[test]
    fn validate_wants_vote() {
        let slot = 123;
        let max_validators = 2;
        let shred_version = rand::rng().random();
        let keypairs = get_keypairs(max_validators, slot);
        let skip = Vote::new_skip_vote(slot);
        let mut partial_cert = PartialCert::new(max_validators);
        let vote = new_invalid_vote(skip, 2);
        assert!(!partial_cert.wants_vote(vote.rank));
        let reward_vote_msg = new_reward_vote_msg(skip, 0, &keypairs, None, shred_version);
        assert!(partial_cert.wants_vote(reward_vote_msg.rank));
        partial_cert.add_vote(&reward_vote_msg).unwrap();
        assert!(!partial_cert.wants_vote(reward_vote_msg.rank));
        let reward_vote_msg = new_reward_vote_msg(skip, 1, &keypairs, None, shred_version);
        partial_cert.add_vote(&reward_vote_msg).unwrap();
        assert!(!partial_cert.wants_vote(reward_vote_msg.rank));
    }
}
