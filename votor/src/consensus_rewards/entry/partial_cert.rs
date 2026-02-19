use {
    super::AddVoteError,
    bitvec::{order::Lsb0, vec::BitVec},
    solana_bls_signatures::{
        Signature as BLSSignature, SignatureCompressed as BLSSignatureCompressed,
        SignatureProjective,
    },
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    solana_signer_store::{encode_base2, EncodeError},
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
    /// number of signatures in the aggregate above.
    cnt: usize,
    validators: Vec<Pubkey>,
}

impl PartialCert {
    /// Returns a new instance of [`PartialCert`].
    pub(super) fn new(max_validators: usize) -> Self {
        Self {
            signature: SignatureProjective::identity(),
            bitvec: BitVec::repeat(false, max_validators),
            cnt: 0,
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
    pub(super) fn add_vote(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        rank: u16,
        signature: &BLSSignature,
    ) -> Result<(), AddVoteError> {
        match self.bitvec.get_mut(rank as usize) {
            None => return Err(AddVoteError::InvalidRank),
            Some(mut ind) => {
                if *ind {
                    return Err(AddVoteError::Duplicate);
                }
                let pubkey = rank_map.get_pubkey_stake_entry(rank.into()).unwrap().pubkey;
                self.validators.push(pubkey);
                self.signature.aggregate_with(std::iter::once(signature))?;
                *ind = true;
            }
        }
        self.cnt = self.cnt.saturating_add(1);
        Ok(())
    }

    /// Builds a signature and associated bitmap from the collected votes.
    ///
    /// On success, returns the built signature, bitmap, and the list of validators in the bitmap.
    pub(super) fn build_sig_bitmap(
        self,
    ) -> Result<(BLSSignatureCompressed, Vec<u8>, Vec<Pubkey>), BuildSigBitmapError> {
        if self.cnt == 0 {
            return Err(BuildSigBitmapError::Empty);
        }
        let mut bitvec = self.bitvec.clone();
        let new_len = bitvec.last_one().map_or(0, |i| i.saturating_add(1));
        bitvec.resize(new_len, false);
        let bitmap = encode_base2(&bitvec).map_err(BuildSigBitmapError::Encode)?;
        let signature = BLSSignature::from(self.signature).try_into().unwrap();
        Ok((signature, bitmap, self.validators))
    }

    /// Returns how many votes have been observed.
    pub(super) fn votes_seen(&self) -> usize {
        self.cnt
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::consensus_rewards::entry::tests::{
            get_rank_map_keypairs, new_vote, validate_bitmap,
        },
        agave_votor_messages::{consensus_message::VoteMessage, vote::Vote},
        solana_bls_signatures::Keypair as BlsKeypair,
    };

    fn new_invalid_vote(vote: Vote, rank: usize) -> VoteMessage {
        let serialized = bincode::serialize(&vote).unwrap();
        let keypair = BlsKeypair::new();
        let signature = keypair.sign(&serialized).into();
        VoteMessage {
            vote,
            signature,
            rank: rank.try_into().unwrap(),
        }
    }

    #[test]
    fn validate_votes_seen() {
        let slot = 123;
        let max_validators = 2;
        let (rank_map, keypairs) = get_rank_map_keypairs(max_validators, slot);
        let skip = Vote::new_skip_vote(7);
        let mut partial_cert = PartialCert::new(max_validators);
        for rank in 0..max_validators {
            let vote = new_vote(skip, rank, &keypairs);
            partial_cert
                .add_vote(&rank_map, vote.rank, &vote.signature)
                .unwrap();
            assert_eq!(partial_cert.votes_seen(), rank + 1);
        }
    }

    #[test]
    fn validate_build_sig_bitmap() {
        let slot = 123;
        let max_validators = 2;
        let (rank_map, keypairs) = get_rank_map_keypairs(max_validators, slot);
        let mut partial_cert = PartialCert::new(max_validators);
        assert!(matches!(
            partial_cert.clone().build_sig_bitmap(),
            Err(BuildSigBitmapError::Empty)
        ));
        let skip = Vote::new_skip_vote(slot);
        for rank in 0..max_validators {
            let vote = new_vote(skip, rank, &keypairs);
            partial_cert
                .add_vote(&rank_map, vote.rank, &vote.signature)
                .unwrap();
            let (_signature, bitmap, _) = partial_cert.clone().build_sig_bitmap().unwrap();
            validate_bitmap(&bitmap, rank + 1, max_validators);
        }
    }

    #[test]
    fn validate_add_vote() {
        let slot = 123;
        let max_validators = 2;
        let (rank_map, keypairs) = get_rank_map_keypairs(max_validators, slot);
        let mut partial_cert = PartialCert::new(max_validators);
        let skip = Vote::new_skip_vote(slot);
        let vote = new_invalid_vote(skip, 2);
        assert!(matches!(
            partial_cert.add_vote(&rank_map, vote.rank, &vote.signature),
            Err(AddVoteError::InvalidRank)
        ));
        let vote = new_vote(skip, 0, &keypairs);
        partial_cert
            .add_vote(&rank_map, vote.rank, &vote.signature)
            .unwrap();
        assert!(matches!(
            partial_cert.add_vote(&rank_map, vote.rank, &vote.signature),
            Err(AddVoteError::Duplicate)
        ));
        let vote = new_vote(skip, 1, &keypairs);
        partial_cert
            .add_vote(&rank_map, vote.rank, &vote.signature)
            .unwrap();
        let vote = new_vote(skip, 0, &keypairs);
        assert!(matches!(
            partial_cert.add_vote(&rank_map, vote.rank, &vote.signature),
            Err(AddVoteError::Duplicate)
        ));
    }

    #[test]
    fn validate_wants_vote() {
        let slot = 123;
        let max_validators = 2;
        let (rank_map, keypairs) = get_rank_map_keypairs(max_validators, slot);
        let skip = Vote::new_skip_vote(slot);
        let mut partial_cert = PartialCert::new(max_validators);
        let vote = new_invalid_vote(skip, 2);
        assert!(!partial_cert.wants_vote(vote.rank));
        let vote = new_vote(skip, 0, &keypairs);
        assert!(partial_cert.wants_vote(vote.rank));
        partial_cert
            .add_vote(&rank_map, vote.rank, &vote.signature)
            .unwrap();
        assert!(!partial_cert.wants_vote(vote.rank));
        let vote = new_vote(skip, 1, &keypairs);
        partial_cert
            .add_vote(&rank_map, vote.rank, &vote.signature)
            .unwrap();
        assert!(!partial_cert.wants_vote(vote.rank));
    }
}
