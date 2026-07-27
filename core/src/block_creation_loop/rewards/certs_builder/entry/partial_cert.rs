use {
    agave_votor::aggregate_accumulator::{AggregateAccumulator, AggregateAccumulatorError},
    agave_votor_messages::{consensus_message::VoteMessage, sig_verified_messages::VoteAggregate},
    solana_bls_signatures::SignatureCompressed as BLSSignatureCompressed,
    solana_pubkey::Pubkey,
    solana_signer_store::EncodeError,
};

pub(super) enum BuildResult {
    Empty,
    EncodingError(EncodeError),
    Success {
        signature: BLSSignatureCompressed,
        bitmap: Vec<u8>,
        validators: Vec<Pubkey>,
    },
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
    ) -> Result<(), AggregateAccumulatorError> {
        self.accumulator.add_aggregate(&aggregate)?;
        self.validators.append(&mut vote_account_pubkeys);
        Ok(())
    }

    /// Accumulates a new observed vote msg from this node.
    pub(super) fn add_own_msg(
        &mut self,
        vote_msg: VoteMessage,
        vote_account_pubkey: Pubkey,
    ) -> Result<(), AggregateAccumulatorError> {
        self.accumulator.add_own_vote_message(&vote_msg)?;
        self.validators.push(vote_account_pubkey);
        Ok(())
    }

    /// Builds a signature and associated bitmap from the collected votes.
    ///
    /// On success, returns the built signature, bitmap, and the list of validators in the bitmap.
    pub(super) fn build_sig_bitmap(self) -> BuildResult {
        if self.validators.is_empty() {
            return BuildResult::Empty;
        }
        let (signature, bitmap) = match self.accumulator.into_sig_and_ranks() {
            Ok(res) => res,
            Err(e) => return BuildResult::EncodingError(e),
        };
        BuildResult::Success {
            signature,
            bitmap,
            validators: self.validators,
        }
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
            BuildResult::Empty
        ));
        let skip = Vote::new_skip_vote(slot);
        for rank in 0..max_validators {
            let (aggregate, vote_account_pubkeys) =
                new_reward_vote_aggregate(skip, rank, &keypairs, None, shred_version);
            partial_cert
                .add_aggregate(aggregate, vote_account_pubkeys)
                .unwrap();
            let BuildResult::Success { bitmap, .. } = partial_cert.clone().build_sig_bitmap()
            else {
                panic!("wrong type");
            };
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
