use {
    crate::sig_verified_messages::VoteAggregate,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::VoteMessage,
        reward_certificate::NUM_SLOTS_FOR_REWARD,
        vote::Vote,
    },
    bitvec::vec::BitVec,
    solana_clock::Slot,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_signer_store::{Decoded, decode},
};

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum RewardInput {
    External(Vec<VoteAggregate>),
    Own(VoteMessage),
    /// Verified certificates from other validators, which can contain votes this node missed.
    Certificates(Vec<Certificate>),
}

#[must_use]
/// Returns true if the given `msg` is needed for rewards.
pub fn rewards_wants_vote(
    my_pubkey: &Pubkey,
    leader_schedule: &LeaderScheduleCache,
    root_slot: Slot,
    vote: &Vote,
) -> bool {
    match vote {
        Vote::Finalize(_)
        | Vote::NotarizeFallback(_)
        | Vote::SkipFallback(_)
        | Vote::Genesis(_) => return false,
        Vote::Notarize(_) | Vote::Skip(_) => (),
    }
    let vote_slot = vote.slot();
    vote_is_relevant_for_rewards(my_pubkey, leader_schedule, root_slot, vote_slot)
}

#[must_use]
/// Returns true if the given certificate is needed for rewards: this node publishes the rewards
/// for its slot, and all of its signers cast the same reward vote.
pub fn rewards_wants_cert(
    my_pubkey: &Pubkey,
    leader_schedule: &LeaderScheduleCache,
    root_bank: &Bank,
    cert_type: &CertificateType,
    bitmap: &[u8],
) -> bool {
    match cert_type {
        CertificateType::Finalize(_) | CertificateType::Genesis(_) => return false,
        CertificateType::Notarize(_)
        | CertificateType::FinalizeFast(_)
        | CertificateType::NotarizeFallback(_)
        | CertificateType::Skip(_) => (),
    }
    let cert_slot = cert_type.slot();
    vote_is_relevant_for_rewards(my_pubkey, leader_schedule, root_bank.slot(), cert_slot)
        && root_bank.get_rank_map(cert_slot).is_some_and(|rank_map| {
            reward_vote_and_signers(cert_type, bitmap, rank_map.len()).is_some()
        })
}

#[must_use]
/// Returns true if a reward vote at the `vote_slot` is needed by this node for rewards.
pub fn vote_is_relevant_for_rewards(
    my_pubkey: &Pubkey,
    leader_schedule: &LeaderScheduleCache,
    root_slot: Slot,
    vote_slot: Slot,
) -> bool {
    if vote_slot.saturating_add(NUM_SLOTS_FOR_REWARD) <= root_slot {
        return false;
    }
    let Some(leader) =
        leader_schedule.slot_leader_at(vote_slot.saturating_add(NUM_SLOTS_FOR_REWARD), None)
    else {
        return false;
    };
    &leader.id == my_pubkey
}

#[must_use]
/// Returns the reward vote that every signer of a certificate cast, and the signers' ranks.
///
/// A certificate that mixes primary and fallback votes returns `None`: its signature cannot be
/// split by vote.
pub fn reward_vote_and_signers(
    cert_type: &CertificateType,
    bitmap: &[u8],
    max_validators: usize,
) -> Option<(Vote, BitVec<u8>)> {
    let vote = match cert_type {
        CertificateType::Notarize(block)
        | CertificateType::FinalizeFast(block)
        | CertificateType::NotarizeFallback(block) => Vote::new_notarization_vote(*block),
        CertificateType::Skip(slot) => Vote::new_skip_vote(*slot),
        CertificateType::Finalize(_) | CertificateType::Genesis(_) => return None,
    };
    let ranks = match decode(bitmap, max_validators).ok()? {
        Decoded::Base2(ranks) => ranks,
        // Base3 certificates are verified against both votes. With no fallback signers, every
        // signer cast the primary vote.
        Decoded::Base3(ranks, fallback)
            if matches!(
                cert_type,
                CertificateType::NotarizeFallback(_) | CertificateType::Skip(_)
            ) && fallback.not_any() =>
        {
            ranks
        }
        Decoded::Base3(_, _) => return None,
    };
    Some((vote, ranks))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor_messages::consensus_message::Block,
        solana_signer_store::{encode_base2, encode_base3},
    };

    #[test]
    fn test_reward_vote_and_signers_requires_a_single_reward_vote() {
        let block = Block::new_unique(10);
        let mut primary = BitVec::<u8>::repeat(false, 3);
        primary.set(0, true);
        let mut fallback = BitVec::<u8>::repeat(false, 3);
        let base2 = encode_base2(&primary).unwrap();
        let primary_only_base3 = encode_base3(&primary, &fallback).unwrap();
        fallback.set(1, true);
        let mixed_base3 = encode_base3(&primary, &fallback).unwrap();
        let reward = |cert_type: &CertificateType, bitmap: &[u8]| {
            reward_vote_and_signers(cert_type, bitmap, 3)
                .map(|(vote, ranks)| (vote, ranks.iter_ones().collect::<Vec<_>>()))
        };

        for cert_type in [
            CertificateType::Notarize(block),
            CertificateType::FinalizeFast(block),
            CertificateType::NotarizeFallback(block),
        ] {
            assert_eq!(
                reward(&cert_type, &base2),
                Some((Vote::new_notarization_vote(block), vec![0])),
            );
        }
        assert_eq!(
            reward(&CertificateType::Skip(block.slot), &base2),
            Some((Vote::new_skip_vote(block.slot), vec![0])),
        );
        for cert_type in [
            CertificateType::Skip(block.slot),
            CertificateType::NotarizeFallback(block),
        ] {
            assert_eq!(
                reward(&cert_type, &primary_only_base3),
                reward(&cert_type, &base2)
            );
            assert_eq!(reward(&cert_type, &mixed_base3), None);
        }
        // Only certificates made of fallback-capable votes may use base3.
        assert_eq!(
            reward(&CertificateType::Notarize(block), &primary_only_base3),
            None
        );
        for cert_type in [
            CertificateType::Finalize(block.slot),
            CertificateType::Genesis(block),
        ] {
            assert_eq!(reward(&cert_type, &base2), None);
        }
    }
}
