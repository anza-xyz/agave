use {
    agave_bls_sigverify::sig_verified_messages::VoteAggregate,
    agave_votor::aggregate_accumulator::{AggregateAccumulator, AggregateAccumulatorError},
    agave_votor_messages::{consensus_message::VoteMessage, stake::Stake},
    bitvec::vec::BitVec,
    solana_bls_signatures::{
        Signature as BLSSignature, SignatureCompressed as BLSSignatureCompressed,
        SignatureProjective,
    },
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    solana_signer_store::{EncodeError, encode_base2},
    std::{cmp::Reverse, sync::LazyLock},
};

/// Maximum number of certificates from other validators kept for each vote. Sigverify sends at
/// most two certificates of each type, and three types carry notarization votes.
const MAX_CERTIFICATES: usize = 6;

/// How reward certs used certificates from other validators.
#[derive(Default)]
pub(in crate::block_creation_loop::rewards) struct CertificateStats {
    /// Validators paid only because a certificate from another validator contains their vote.
    pub(in crate::block_creation_loop::rewards) validators_from_certs: u64,
    /// Certificates that would pay more validators, but that partly overlap a collected vote
    /// aggregate, which cannot be split.
    pub(in crate::block_creation_loop::rewards) partially_overlapping_certs: u64,
}

/// A verified certificate from another validator.
#[derive(Clone)]
struct ExternalCert {
    ranks: BitVec<u8>,
    signature: BLSSignature,
}

pub(super) enum BuildResult {
    Empty,
    Identity,
    EncodingError(EncodeError),
    Success {
        signature: BLSSignatureCompressed,
        bitmap: Vec<u8>,
        validators: Vec<Pubkey>,
    },
}

static IDENTITY_SIGNATURE: LazyLock<BLSSignatureCompressed> =
    LazyLock::new(|| SignatureProjective::identity().into());

#[derive(Clone)]
/// Struct to hold state for building a single reward cert.
pub(super) struct PartialCert {
    accumulator: AggregateAccumulator,
    validators: Vec<Pubkey>,
    max_validators: usize,
    /// The vote aggregates in `accumulator`, kept to add the ones that a certificate is missing.
    aggregates: Vec<VoteAggregate>,
    /// Verified certificates from other validators.
    certificates: Vec<ExternalCert>,
}

impl PartialCert {
    /// Returns a new instance of [`PartialCert`].
    pub(super) fn new(max_validators: usize) -> Self {
        Self {
            accumulator: AggregateAccumulator::new(max_validators),
            validators: Vec::with_capacity(max_validators),
            max_validators,
            aggregates: Vec::new(),
            certificates: Vec::new(),
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
        self.aggregates.push(aggregate);
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
        self.aggregates.push(VoteAggregate::new_from_verified_vote(
            self.max_validators,
            vote_msg,
        ));
        Ok(())
    }

    /// Stores a verified certificate from another validator, signed by `cert_ranks`.
    pub(super) fn add_certificate(&mut self, cert_ranks: &BitVec<u8>, signature: BLSSignature) {
        if self.certificates.len() >= MAX_CERTIFICATES {
            return;
        }
        // A certificate's bitmap ends at its last signer. Copy the signers into a set as wide as
        // the rank map, so that any rank of a collected aggregate can index it.
        let mut ranks = BitVec::repeat(false, self.max_validators);
        for rank in cert_ranks.iter_ones() {
            if rank >= self.max_validators {
                return;
            }
            ranks.set(rank, true);
        }
        if ranks.any() && self.certificates.iter().all(|cert| cert.ranks != ranks) {
            self.certificates.push(ExternalCert { ranks, signature });
        }
    }

    /// Builds the reward cert from the collected votes.
    ///
    /// A certificate from another validator can contain votes that never reached this node. If
    /// one pays additional stake, it is combined with the collected votes that it is missing.
    pub(super) fn build(
        self,
        rank_map: &BLSPubkeyToRankMap,
        stats: &mut CertificateStats,
    ) -> BuildResult {
        match self.build_from_certificates(rank_map, stats) {
            Some(result) => result,
            None => self.build_sig_bitmap(),
        }
    }

    /// Returns the certificate that pays the most additional stake once combined with the
    /// collected votes, if any.
    fn build_from_certificates(
        &self,
        rank_map: &BLSPubkeyToRankMap,
        stats: &mut CertificateStats,
    ) -> Option<BuildResult> {
        if self.certificates.is_empty() || rank_map.len() != self.max_validators {
            return None;
        }
        let mut collected = BitVec::<u8>::repeat(false, self.max_validators);
        for aggregate in &self.aggregates {
            collected |= aggregate.ranks();
        }
        let mut certificates = self
            .certificates
            .iter()
            .filter_map(|cert| {
                let added_stake = cert
                    .ranks
                    .iter_ones()
                    .filter(|&rank| !collected[rank])
                    .filter_map(|rank| rank_map.get_pubkey_stake_entry(rank))
                    .fold(0u64, |stake, entry| {
                        stake.saturating_add(entry.stake.get().get())
                    });
                (added_stake > 0).then_some((added_stake, cert))
            })
            .collect::<Vec<_>>();
        certificates.sort_unstable_by_key(|&(added_stake, _)| Reverse(added_stake));
        for (_, cert) in certificates {
            let Some((signature, mut ranks)) = self.combine(cert, stats) else {
                continue;
            };
            // As in `build_sig_bitmap`, an identity aggregate is never packed.
            let signature = BLSSignatureCompressed::from(signature);
            if signature == *IDENTITY_SIGNATURE {
                continue;
            }
            let validators = ranks
                .iter_ones()
                .map(|rank| {
                    rank_map
                        .get_pubkey_stake_entry(rank)
                        .map(|entry| entry.vote_account_pubkey)
                })
                .collect::<Option<Vec<_>>>()?;
            ranks.resize(
                ranks.last_one().map_or(0, |rank| rank.saturating_add(1)),
                false,
            );
            let bitmap = match encode_base2(&ranks) {
                Ok(bitmap) => bitmap,
                Err(e) => return Some(BuildResult::EncodingError(e)),
            };
            stats.validators_from_certs +=
                validators.len().saturating_sub(collected.count_ones()) as u64;
            return Some(BuildResult::Success {
                signature,
                bitmap,
                validators,
            });
        }
        None
    }

    /// Combines `cert` with the collected aggregates that do not overlap it, and returns the
    /// signature and the signers.
    ///
    /// The certificate and each aggregate were verified, and their signers are disjoint, so the
    /// combination verifies too. An aggregate was only verified as a whole, so `cert` cannot be
    /// used if an aggregate partly overlaps it.
    fn combine(
        &self,
        cert: &ExternalCert,
        stats: &mut CertificateStats,
    ) -> Option<(SignatureProjective, BitVec<u8>)> {
        let mut signature = SignatureProjective::identity();
        signature
            .aggregate_with(std::iter::once(&cert.signature))
            .ok()?;
        let mut ranks = cert.ranks.clone();
        for aggregate in &self.aggregates {
            let signers = aggregate.ranks();
            if signers.iter_ones().all(|rank| !cert.ranks[rank]) {
                signature
                    .aggregate_with(std::iter::once(aggregate.signature()))
                    .ok()?;
                ranks |= signers;
            } else if signers.iter_ones().any(|rank| !cert.ranks[rank]) {
                stats.partially_overlapping_certs += 1;
                return None;
            }
        }
        Some((signature, ranks))
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

        // In case of a aggregate with low participation, malicious validators can collude
        // such that the aggregate signature ends up being the identity signature.
        // The individual votes would be valid, but their aggregate would be invalid.
        // To protect against this, we do not pack such rewards aggregates.
        //
        // Note: this does not affect normal certificate construction, as that requires at least
        // 60% stake participation. For such a certificate to aggregate to the identity signature,
        // all 60% must be malicious.
        if signature == *IDENTITY_SIGNATURE {
            return BuildResult::Identity;
        }
        BuildResult::Success {
            signature,
            bitmap,
            validators: self.validators,
        }
    }

    /// Returns how much stake has been observed.
    pub(super) fn stake(&self) -> Stake {
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
