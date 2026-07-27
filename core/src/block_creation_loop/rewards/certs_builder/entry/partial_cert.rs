use {
    crate::block_creation_loop::rewards::certs_builder::entry::AddAggregateError,
    agave_votor_messages::{
        aggregate_accumulator::{AggregateAccumulator, AggregateAccumulatorError},
        consensus_message::VoteMessage,
        sig_verified_messages::VoteAggregate,
    },
    solana_bls_signatures::SignatureCompressed as BLSSignatureCompressed,
    solana_pubkey::Pubkey,
    thiserror::Error,
};

/// Different types of errors that can be returned from building signature and the associated bitmap.
#[derive(Debug, Error)]
pub(super) enum BuildSigBitmapError {
    #[error("Empty bitvec")]
    Empty,
    #[error("AggregateAccumulator failed with {0}")]
    Accumulating(#[from] AggregateAccumulatorError),
}

#[derive(Clone)]
/// Struct to hold state for building a single reward cert.
pub(super) struct PartialCert {
    accumulator: AggregateAccumulator,
    validators: Vec<Pubkey>,
}

impl PartialCert {
    /// Returns a new instance of [`PartialCert`].
    pub(super) fn new(max_validators: usize) -> Self {
        Self {
            accumulator: AggregateAccumulator::new(max_validators),
            validators: Vec::with_capacity(max_validators),
        }
    }

    /// Accumulates a new observed vote aggregate from another validator.
    pub(super) fn add_aggregate(
        &mut self,
        aggregate: VoteAggregate,
        mut vote_account_pubkeys: Vec<Pubkey>,
    ) -> Result<(), AddAggregateError> {
        self.accumulator.add_aggregate(&aggregate)?;
        self.validators.append(&mut vote_account_pubkeys);
        Ok(())
    }

    /// Accumulates a new observed vote msg from this node.
    pub(super) fn add_own_msg(
        &mut self,
        vote_msg: VoteMessage,
        vote_account_pubkey: Pubkey,
    ) -> Result<(), AddAggregateError> {
        self.accumulator.add_own_vote_message(&vote_msg)?;
        self.validators.push(vote_account_pubkey);
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
        let (signature, ranks) = self.accumulator.into_sig_and_ranks()?;
        Ok((signature, ranks, self.validators))
    }

    /// Returns how much stake has been observed.
    pub(super) fn stake(&self) -> u64 {
        self.accumulator.stake()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::block_creation_loop::rewards::certs_builder::entry::tests::{
            get_keypairs, new_reward_vote_aggregate, validate_bitmap,
        },
        agave_votor_messages::vote::Vote,
        rand::Rng,
    };

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
            let (aggregate, vote_account_pubkeys) =
                new_reward_vote_aggregate(skip, rank, &keypairs, None, shred_version);
            partial_cert
                .add_aggregate(aggregate, vote_account_pubkeys)
                .unwrap();
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
        let skip = Vote::new_skip_vote(slot);
        let (aggregate, vote_account_pubkeys) =
            new_reward_vote_aggregate(skip, 0, &keypairs, None, shred_version);
        partial_cert
            .add_aggregate(aggregate, vote_account_pubkeys)
            .unwrap();
        let (aggregate, vote_account_pubkeys) =
            new_reward_vote_aggregate(skip, 1, &keypairs, None, shred_version);
        partial_cert
            .add_aggregate(aggregate, vote_account_pubkeys)
            .unwrap();
    }
}
