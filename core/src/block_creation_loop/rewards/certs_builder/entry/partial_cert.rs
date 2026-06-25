use {
    super::AddVoteError,
    agave_bls_sigverify::sig_verified_messages::SigVerifiedVoteBatch,
    bitvec::{order::Lsb0, vec::BitVec},
    solana_bls_signatures::{
        Signature as BLSSignature, SignatureCompressed as BLSSignatureCompressed,
        SignatureProjective,
    },
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
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
    pub(super) fn wants_vote(&self, ranks: &BitVec<u8>) -> bool {
        !self
            .bitvec
            .iter_ones()
            .any(|i| ranks.get(i).is_some_and(|bit| *bit))
    }

    /// Adds a new observed vote to the aggregate.
    pub(super) fn add_vote(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        vote: &SigVerifiedVoteBatch,
    ) -> Result<(), AddVoteError> {
        if !self.wants_vote(vote.ranks()) {
            return Err(AddVoteError::Duplicate);
        }
        let mut signature = self.signature;
        signature.aggregate_with(std::iter::once(vote.signature()))?;
        let mut validators = vec![];
        for rank in vote.ranks().iter_ones() {
            let entry = rank_map
                .get_pubkey_stake_entry(rank)
                .ok_or(AddVoteError::InvalidRank)?;
            validators.push(entry.vote_account_pubkey);
        }
        self.bitvec |= vote.ranks();
        self.validators.append(&mut validators);
        self.stake += vote.stake().get();
        self.signature = signature;
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
            get_rank_map_keypairs, new_vote, validate_bitmap,
        },
        agave_votor_messages::vote::Vote,
        rand::Rng,
    };

    #[test]
    fn validate_build_sig_bitmap() {
        let slot = 123;
        let max_validators = 2;
        let shred_version = rand::rng().random();
        let (rank_map, keypairs, bank, _forks) = get_rank_map_keypairs(max_validators, slot);
        let mut partial_cert = PartialCert::new(max_validators);
        assert!(matches!(
            partial_cert.clone().build_sig_bitmap(),
            Err(BuildSigBitmapError::Empty)
        ));
        let skip = Vote::new_skip_vote(slot);
        for rank in 0..max_validators {
            let vote = new_vote(&bank, skip, rank, &keypairs, shred_version);
            partial_cert.add_vote(&rank_map, &vote).unwrap();
            let (_signature, bitmap, _) = partial_cert.clone().build_sig_bitmap().unwrap();
            validate_bitmap(&bitmap, rank + 1, max_validators);
        }
    }

    #[test]
    fn validate_add_vote() {
        let slot = 123;
        let max_validators = 2;
        let shred_version = rand::rng().random();
        let (rank_map, keypairs, bank, _forks) = get_rank_map_keypairs(max_validators, slot);
        let mut partial_cert = PartialCert::new(max_validators);
        let skip = Vote::new_skip_vote(slot);
        let vote = new_vote(&bank, skip, 0, &keypairs, shred_version);
        partial_cert.add_vote(&rank_map, &vote).unwrap();
        assert!(matches!(
            partial_cert.add_vote(&rank_map, &vote),
            Err(AddVoteError::Duplicate)
        ));
        let vote = new_vote(&bank, skip, 1, &keypairs, shred_version);
        partial_cert.add_vote(&rank_map, &vote).unwrap();
        let vote = new_vote(&bank, skip, 0, &keypairs, shred_version);
        assert!(matches!(
            partial_cert.add_vote(&rank_map, &vote),
            Err(AddVoteError::Duplicate)
        ));
    }

    #[test]
    fn validate_wants_vote() {
        let slot = 123;
        let max_validators = 2;
        let shred_version = rand::rng().random();
        let (rank_map, keypairs, bank, _forks) = get_rank_map_keypairs(max_validators, slot);
        let skip = Vote::new_skip_vote(slot);
        let mut partial_cert = PartialCert::new(max_validators);
        let vote = new_vote(&bank, skip, 0, &keypairs, shred_version);
        assert!(partial_cert.wants_vote(vote.ranks()));
        partial_cert.add_vote(&rank_map, &vote).unwrap();
        assert!(!partial_cert.wants_vote(vote.ranks()));
        let vote = new_vote(&bank, skip, 1, &keypairs, shred_version);
        partial_cert.add_vote(&rank_map, &vote).unwrap();
        assert!(!partial_cert.wants_vote(vote.ranks()));
    }
}
