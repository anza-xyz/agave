use {
    crate::byzzfuzz::interceptor::AlpenglowInterceptorState,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::Block,
        fraction::Fraction,
        vote::Vote,
    },
    log::info,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_signer_store::{Decoded, decode},
    std::{
        collections::{BTreeMap, HashMap, HashSet},
        num::NonZeroU64,
    },
};

// Emits a per-run coverage summary: the distribution of vote kinds (with unique
// signer counts) and certificate kinds actually observed on the wire. Combined
// with the mutation histogram, this shows which protocol surface a run reached
// and which certificate/vote types stay unproduced under random schedules.
pub(crate) fn report_coverage(test_name: &str, data: &AlpenglowInterceptorState) {
    let mut vote_kinds: BTreeMap<&'static str, (u64, HashSet<Pubkey>)> = BTreeMap::new();
    for (source, _, vote_message) in &data.votes {
        let kind = match &vote_message.vote {
            Vote::Notarize(_) => "Notarize",
            Vote::NotarizeFallback(_) => "NotarizeFallback",
            Vote::Finalize(_) => "Finalize",
            Vote::Skip(_) => "Skip",
            Vote::SkipFallback(_) => "SkipFallback",
            Vote::Genesis(_) => "Genesis",
        };
        let entry = vote_kinds.entry(kind).or_default();
        entry.0 += 1;
        entry.1.insert(*source);
    }
    let vote_summary = vote_kinds
        .iter()
        .map(|(kind, (count, sources))| (*kind, *count, sources.len()))
        .collect::<Vec<_>>();

    let mut cert_kinds: BTreeMap<&'static str, u64> = BTreeMap::new();
    for (_, _, certificate) in &data.certificates {
        let kind = match certificate.cert_type {
            CertificateType::Finalize(_) => "Finalize",
            CertificateType::FinalizeFast(_) => "FinalizeFast",
            CertificateType::Notarize(_) => "Notarize",
            CertificateType::NotarizeFallback(_) => "NotarizeFallback",
            CertificateType::Skip(_) => "Skip",
            CertificateType::Genesis(_) => "Genesis",
        };
        *cert_kinds.entry(kind).or_default() += 1;
    }
    info!(
        "{test_name}: byzfuzz coverage votes total={} kinds(count,uniq_sources)={:?}",
        data.votes.len(),
        vote_summary,
    );
    info!(
        "{test_name}: byzfuzz coverage certs total={} kinds={:?}",
        data.certificates.len(),
        cert_kinds,
    );
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum VoteKind {
    Notarize,
    Finalize,
    Skip,
    NotarizeFallback,
    SkipFallback,
    Genesis,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum VotePayload {
    Block(Block),
    Slot(Slot),
}

// Runs all byzfuzz invariants over the captured interceptor state.
pub fn validate_invariants(
    data: &AlpenglowInterceptorState,
    byzantine_sources: &HashSet<Pubkey>,
    source_stakes: &HashMap<Pubkey, u64>,
    min_certified_slot: Slot,
) {
    validate_certified_slot_progress(data, source_stakes, min_certified_slot);
    validate_certificate_thresholds(data, source_stakes);
    validate_no_conflicting_finalization_and_nullification(data);
    validate_no_quorum_notarizations_for_multiple_digests(data, source_stakes);
    validate_finalizations_are_notarized(data, source_stakes);
    validate_no_conflicting_notarization_after_finalization(data, source_stakes);
    validate_no_quorum_finalizations_for_multiple_digests(data, source_stakes);
    validate_correct_nodes_do_not_double_sign(data, byzantine_sources);
}

// Quorum threshold (percent of total stake) needed to certify slot progress:
// notarize, finalize, and skip certificates all require 60%.
const PROGRESS_THRESHOLD_PERCENT: u128 = 60;

// Ensures every slot up to the waited root slot made certifiable progress.
//
// The cluster has already rooted `min_certified_slot`, so every slot below it
// is decided.  We confirm we *witnessed* that: a slot counts as progress if we
// observed a progress certificate for it, or — since certificate broadcast is
// best-effort and a rooted slot may have no cert on the wire — if the votes we
// recorded reconstruct a notarize/finalize/skip quorum.  Votes are sent
// all-to-all, so they are observed far more reliably than certificates.
fn validate_certified_slot_progress(
    data: &AlpenglowInterceptorState,
    source_stakes: &HashMap<Pubkey, u64>,
    min_certified_slot: Slot,
) {
    let certified_slots = data
        .certificates
        .iter()
        .filter_map(|(_, _, certificate)| certified_slot(certificate.cert_type))
        .collect::<HashSet<_>>();
    let quorum_slots = slots_with_vote_quorum(data, source_stakes);
    let missing_slots = (1..=min_certified_slot)
        .filter(|slot| !certified_slots.contains(slot) && !quorum_slots.contains(slot))
        .collect::<Vec<_>>();
    assert!(
        missing_slots.is_empty(),
        "byzfuzz invariant failed: missing progress certificates up to {min_certified_slot}: \
         {missing_slots:?}",
    );
}

// Slots whose recorded votes reconstruct a progress quorum: a single block
// notarized (notarize, optionally with fallback) by >=60% stake, or >=60%
// finalize votes, or >=60% skip (skip or skip-fallback) votes.  Each voter's
// stake is counted once even though the same vote reaches many destinations.
fn slots_with_vote_quorum(
    data: &AlpenglowInterceptorState,
    source_stakes: &HashMap<Pubkey, u64>,
) -> HashSet<Slot> {
    #[derive(Default)]
    struct SlotVotes {
        notarize_by_block: HashMap<Block, HashSet<Pubkey>>,
        finalize: HashSet<Pubkey>,
        skip: HashSet<Pubkey>,
    }

    let mut by_slot = HashMap::<Slot, SlotVotes>::new();
    for (source, _, vote_message) in &data.votes {
        let slot = vote_message.vote.slot();
        let entry = by_slot.entry(slot).or_default();
        match &vote_message.vote {
            // Notarize and notarize-fallback both certify the same block.
            Vote::Notarize(vote) => {
                entry
                    .notarize_by_block
                    .entry(vote.block)
                    .or_default()
                    .insert(*source);
            }
            Vote::NotarizeFallback(vote) => {
                entry
                    .notarize_by_block
                    .entry(vote.block)
                    .or_default()
                    .insert(*source);
            }
            Vote::Finalize(_) => {
                entry.finalize.insert(*source);
            }
            // Skip and skip-fallback both certify a skip.
            Vote::Skip(_) | Vote::SkipFallback(_) => {
                entry.skip.insert(*source);
            }
            Vote::Genesis(_) => {}
        }
    }

    let total_stake = source_stakes
        .values()
        .map(|stake| *stake as u128)
        .sum::<u128>();
    let stake_of = |sources: &HashSet<Pubkey>| -> u128 {
        sources
            .iter()
            .map(|source| source_stakes.get(source).copied().unwrap_or(0) as u128)
            .sum()
    };
    let meets_quorum = |stake: u128| stake * 100 >= PROGRESS_THRESHOLD_PERCENT * total_stake;

    by_slot
        .into_iter()
        .filter_map(|(slot, votes)| {
            let notarized = votes
                .notarize_by_block
                .values()
                .any(|sources| meets_quorum(stake_of(sources)));
            let finalized = meets_quorum(stake_of(&votes.finalize));
            let skipped = meets_quorum(stake_of(&votes.skip));
            (notarized || finalized || skipped).then_some(slot)
        })
        .collect()
}

// Ensures every certificate carries enough signing stake for its threshold.
fn validate_certificate_thresholds(
    data: &AlpenglowInterceptorState,
    source_stakes: &HashMap<Pubkey, u64>,
) {
    let rank_stakes = rank_stakes(data, source_stakes);
    let total_stake = source_stakes.values().sum::<u64>();
    let total_stake = NonZeroU64::new(total_stake).expect("byzfuzz stake must be nonzero");
    for (_, _, certificate) in &data.certificates {
        let summary = certificate_signer_summary(certificate, data.validator_count, &rank_stakes);
        let actual = Fraction::new(summary.stake, total_stake);
        let required = certificate.cert_type.limits_and_vote_types().0;
        let cert_type = &certificate.cert_type;
        assert!(
            summary.users > 0,
            "byzfuzz invariant failed: {cert_type:?} has no signers",
        );
        assert!(
            summary.users <= data.validator_count,
            "byzfuzz invariant failed: {cert_type:?} has {} signers but only {} validators",
            summary.users,
            data.validator_count,
        );
        assert!(
            actual >= required,
            "byzfuzz invariant failed: {cert_type:?} stake {actual} from {} unique signers, needs \
             {required}",
            summary.users,
        );
    }
}

// In this implementation, a skip certificate is the nullification certificate
// for a view/slot. No slot may be both finalized and nullified.
fn validate_no_conflicting_finalization_and_nullification(data: &AlpenglowInterceptorState) {
    let finalized_slots = finalized_slots(data);
    let nullified_slots = nullified_slots(data);
    for slot in finalized_slots.intersection(&nullified_slots) {
        panic!(
            "byzfuzz invariant failed: slot {slot} has finalization and nullification/skip \
             certificates",
        );
    }
}

// Ensures a view cannot have notarization-family quorums for multiple digests.
fn validate_no_quorum_notarizations_for_multiple_digests(
    data: &AlpenglowInterceptorState,
    source_stakes: &HashMap<Pubkey, u64>,
) {
    let notarized_by_slot = quorum_notarized_blocks_by_slot(data, source_stakes);
    for (slot, blocks) in notarized_by_slot {
        assert!(
            blocks.len() <= 1,
            "byzfuzz invariant failed: slot {slot} has quorum notarizations for multiple \
             payloads: {blocks:?}",
        );
    }
}

// Ensures every finalization has a matching notarization witness.
fn validate_finalizations_are_notarized(
    data: &AlpenglowInterceptorState,
    source_stakes: &HashMap<Pubkey, u64>,
) {
    let notarized_by_slot = quorum_notarized_blocks_by_slot(data, source_stakes);

    for (_, _, certificate) in &data.certificates {
        match certificate.cert_type {
            CertificateType::Finalize(slot) => {
                let blocks = notarized_by_slot.get(&slot).unwrap_or_else(|| {
                    panic!("byzfuzz invariant failed: Finalize({slot}) has no notarization")
                });
                assert!(
                    blocks.len() == 1,
                    "byzfuzz invariant failed: Finalize({slot}) has multiple notarized payloads: \
                     {blocks:?}",
                );
            }
            CertificateType::FinalizeFast(block) => assert!(
                notarized_by_slot
                    .get(&block.slot)
                    .is_some_and(|blocks| blocks.contains(&block)),
                "byzfuzz invariant failed: FinalizeFast({block:?}) has no matching notarization"
            ),
            _ => {}
        }
    }
}

// Ensures finalized slots do not have notarizations for conflicting blocks.
fn validate_no_conflicting_notarization_after_finalization(
    data: &AlpenglowInterceptorState,
    source_stakes: &HashMap<Pubkey, u64>,
) {
    let notarized_by_slot = quorum_notarized_blocks_by_slot(data, source_stakes);
    for (slot, finalized_block) in finalized_blocks_by_slot(data, &notarized_by_slot) {
        if let Some(notarized_blocks) = notarized_by_slot.get(&slot) {
            for block in notarized_blocks {
                assert_eq!(
                    *block, finalized_block,
                    "byzfuzz invariant failed: slot {slot} finalized {finalized_block:?} but \
                     notarized {block:?}",
                );
            }
        }
    }
}

// Ensures all finalization certificates for a slot agree on the same block.
fn validate_no_quorum_finalizations_for_multiple_digests(
    data: &AlpenglowInterceptorState,
    source_stakes: &HashMap<Pubkey, u64>,
) {
    let notarized_by_slot = quorum_notarized_blocks_by_slot(data, source_stakes);
    let mut finalized_by_slot = HashMap::<Slot, Block>::new();

    for (_, _, certificate) in &data.certificates {
        match certificate.cert_type {
            CertificateType::Finalize(slot) => {
                if let Some(block) = only_notarized_block(slot, &notarized_by_slot) {
                    record_finalized_block(&mut finalized_by_slot, slot, block);
                }
            }
            CertificateType::FinalizeFast(block) => {
                record_finalized_block(&mut finalized_by_slot, block.slot, block);
            }
            _ => {}
        }
    }
}

// Ensures honest nodes do not sign two payloads for one vote kind and slot.
fn validate_correct_nodes_do_not_double_sign(
    data: &AlpenglowInterceptorState,
    byzantine_sources: &HashSet<Pubkey>,
) {
    let mut votes_by_source_kind_slot = HashMap::<(Pubkey, VoteKind, Slot), VotePayload>::new();
    for (source, _, vote_message) in &data.votes {
        if byzantine_sources.contains(source) {
            continue;
        }
        let (kind, slot, payload) = vote_parts(&vote_message.vote);
        let key = (*source, kind, slot);
        if let Some(existing) = votes_by_source_kind_slot.get(&key) {
            assert_eq!(
                *existing, payload,
                "byzfuzz invariant failed: correct node {source} signed multiple {kind:?} \
                 payloads in slot {slot}: {existing:?} and {payload:?}",
            );
        } else {
            votes_by_source_kind_slot.insert(key, payload);
        }
    }
}

fn finalized_blocks_by_slot(
    data: &AlpenglowInterceptorState,
    notarized_by_slot: &HashMap<Slot, HashSet<Block>>,
) -> HashMap<Slot, Block> {
    let mut finalized_by_slot = HashMap::<Slot, Block>::new();
    for (_, _, certificate) in &data.certificates {
        match certificate.cert_type {
            CertificateType::Finalize(slot) => {
                if let Some(block) = only_notarized_block(slot, notarized_by_slot) {
                    record_finalized_block(&mut finalized_by_slot, slot, block);
                }
            }
            CertificateType::FinalizeFast(block) => {
                record_finalized_block(&mut finalized_by_slot, block.slot, block);
            }
            _ => {}
        }
    }
    finalized_by_slot
}

fn finalized_slots(data: &AlpenglowInterceptorState) -> HashSet<Slot> {
    data.certificates
        .iter()
        .filter_map(|(_, _, certificate)| match certificate.cert_type {
            CertificateType::Finalize(slot) => Some(slot),
            CertificateType::FinalizeFast(block) => Some(block.slot),
            _ => None,
        })
        .collect()
}

fn nullified_slots(data: &AlpenglowInterceptorState) -> HashSet<Slot> {
    data.certificates
        .iter()
        .filter_map(|(_, _, certificate)| match certificate.cert_type {
            CertificateType::Skip(slot) => Some(slot),
            _ => None,
        })
        .collect()
}

fn notarized_blocks(data: &AlpenglowInterceptorState) -> HashSet<Block> {
    data.certificates
        .iter()
        .filter_map(|(_, _, certificate)| match certificate.cert_type {
            CertificateType::Notarize(block)
            | CertificateType::NotarizeFallback(block)
            | CertificateType::FinalizeFast(block) => Some(block),
            _ => None,
        })
        .collect()
}

fn notarized_blocks_by_slot(data: &AlpenglowInterceptorState) -> HashMap<Slot, HashSet<Block>> {
    let mut notarized_by_slot = HashMap::<Slot, HashSet<Block>>::new();
    for block in notarized_blocks(data) {
        notarized_by_slot
            .entry(block.slot)
            .or_default()
            .insert(block);
    }
    notarized_by_slot
}

fn quorum_notarized_blocks_by_slot(
    data: &AlpenglowInterceptorState,
    source_stakes: &HashMap<Pubkey, u64>,
) -> HashMap<Slot, HashSet<Block>> {
    let mut notarized_by_slot = notarized_blocks_by_slot(data);
    for (block, sources) in notarization_vote_sources_by_block(data) {
        if sources_have_quorum(&sources, source_stakes) {
            notarized_by_slot
                .entry(block.slot)
                .or_default()
                .insert(block);
        }
    }
    notarized_by_slot
}

fn notarization_vote_sources_by_block(
    data: &AlpenglowInterceptorState,
) -> HashMap<Block, HashSet<Pubkey>> {
    let mut sources_by_block = HashMap::<Block, HashSet<Pubkey>>::new();
    for (source, _, vote_message) in &data.votes {
        match vote_message.vote {
            Vote::Notarize(vote) => {
                sources_by_block
                    .entry(vote.block)
                    .or_default()
                    .insert(*source);
            }
            Vote::NotarizeFallback(vote) => {
                sources_by_block
                    .entry(vote.block)
                    .or_default()
                    .insert(*source);
            }
            _ => {}
        }
    }
    sources_by_block
}

fn sources_have_quorum(sources: &HashSet<Pubkey>, source_stakes: &HashMap<Pubkey, u64>) -> bool {
    let total_stake = source_stakes
        .values()
        .map(|stake| *stake as u128)
        .sum::<u128>();
    let signing_stake = sources
        .iter()
        .map(|source| source_stakes.get(source).copied().unwrap_or(0) as u128)
        .sum::<u128>();
    signing_stake * 100 >= PROGRESS_THRESHOLD_PERCENT * total_stake
}

// Progress evidence per slot: finalized, skipped, or notarized.  Notarized
// slots can root through a finalized descendant without ever getting their
// own finalize certificate, so notarization counts.
fn certified_slot(cert_type: CertificateType) -> Option<Slot> {
    match cert_type {
        CertificateType::Finalize(slot) | CertificateType::Skip(slot) => Some(slot),
        CertificateType::FinalizeFast(block)
        | CertificateType::Notarize(block)
        | CertificateType::NotarizeFallback(block) => Some(block.slot),
        CertificateType::Genesis(_) => None,
    }
}

fn only_notarized_block(
    slot: Slot,
    notarized_by_slot: &HashMap<Slot, HashSet<Block>>,
) -> Option<Block> {
    let blocks = notarized_by_slot.get(&slot)?;
    assert!(
        blocks.len() == 1,
        "byzfuzz invariant failed: Finalize({slot}) has multiple notarized payloads: {blocks:?}",
    );
    blocks.iter().copied().next()
}

fn record_finalized_block(finalized_by_slot: &mut HashMap<Slot, Block>, slot: Slot, block: Block) {
    if let Some(existing) = finalized_by_slot.get(&slot) {
        assert_eq!(
            *existing, block,
            "byzfuzz invariant failed: slot {slot} finalized multiple payloads: {existing:?} and \
             {block:?}",
        );
    } else {
        finalized_by_slot.insert(slot, block);
    }
}

#[derive(Debug)]
struct CertificateSignerSummary {
    stake: u64,
    users: usize,
}

fn certificate_signer_summary(
    certificate: &Certificate,
    validator_count: usize,
    rank_stakes: &HashMap<u16, u64>,
) -> CertificateSignerSummary {
    let cert_type = &certificate.cert_type;
    match decode(&certificate.bitmap, validator_count).unwrap_or_else(|err| {
        panic!("byzfuzz invariant failed: failed to decode {cert_type:?}: {err:?}",)
    }) {
        Decoded::Base2(signers) => signer_summary(signers.iter_ones(), cert_type, rank_stakes),
        Decoded::Base3(primary, fallback) => {
            assert_eq!(
                primary.len(),
                fallback.len(),
                "byzfuzz invariant failed: {cert_type:?} has mismatched Base3 bitmap lengths",
            );
            let mut seen = HashSet::<usize>::new();
            let mut ranks = Vec::new();
            for rank in primary.iter_ones() {
                assert!(
                    seen.insert(rank),
                    "byzfuzz invariant failed: {cert_type:?} has duplicate signer rank {rank}",
                );
                ranks.push(rank);
            }
            for rank in fallback.iter_ones() {
                assert!(
                    seen.insert(rank),
                    "byzfuzz invariant failed: {cert_type:?} has signer rank {rank} in both Base3 \
                     bitmaps",
                );
                ranks.push(rank);
            }
            signer_summary(ranks.into_iter(), cert_type, rank_stakes)
        }
    }
}

fn signer_summary(
    ranks: impl Iterator<Item = usize>,
    cert_type: &CertificateType,
    rank_stakes: &HashMap<u16, u64>,
) -> CertificateSignerSummary {
    let mut summary = CertificateSignerSummary { stake: 0, users: 0 };
    for rank in ranks {
        let rank = u16::try_from(rank).unwrap_or_else(|_| {
            panic!("byzfuzz invariant failed: {cert_type:?} signer rank {rank} exceeds u16")
        });
        summary.stake += *rank_stakes.get(&rank).unwrap_or_else(|| {
            panic!("byzfuzz invariant failed: {cert_type:?} missing stake for rank {rank}")
        });
        summary.users += 1;
    }
    summary
}

fn rank_stakes(
    data: &AlpenglowInterceptorState,
    source_stakes: &HashMap<Pubkey, u64>,
) -> HashMap<u16, u64> {
    let mut rank_stakes = HashMap::<u16, u64>::new();
    let mut rank_sources = HashMap::<u16, Pubkey>::new();
    for (source, _, vote_message) in &data.votes {
        let stake = *source_stakes
            .get(source)
            .unwrap_or_else(|| panic!("byzfuzz invariant failed: missing stake for {source}"));
        if let Some(existing_source) = rank_sources.insert(vote_message.rank, *source) {
            assert_eq!(
                existing_source, *source,
                "byzfuzz invariant failed: rank {} mapped to multiple sources: {} and {}",
                vote_message.rank, existing_source, source,
            );
        }
        if let Some(existing) = rank_stakes.insert(vote_message.rank, stake) {
            assert_eq!(
                existing, stake,
                "byzfuzz invariant failed: rank {} mapped to multiple stakes: {existing} and \
                 {stake}",
                vote_message.rank
            );
        }
    }
    rank_stakes
}

fn vote_parts(vote: &Vote) -> (VoteKind, Slot, VotePayload) {
    match vote {
        Vote::Notarize(vote) => (
            VoteKind::Notarize,
            vote.block.slot,
            VotePayload::Block(vote.block),
        ),
        Vote::Finalize(vote) => (VoteKind::Finalize, vote.slot, VotePayload::Slot(vote.slot)),
        Vote::Skip(vote) => (VoteKind::Skip, vote.slot, VotePayload::Slot(vote.slot)),
        Vote::NotarizeFallback(vote) => (
            VoteKind::NotarizeFallback,
            vote.block.slot,
            VotePayload::Block(vote.block),
        ),
        Vote::SkipFallback(vote) => (
            VoteKind::SkipFallback,
            vote.slot,
            VotePayload::Slot(vote.slot),
        ),
        Vote::Genesis(vote) => (
            VoteKind::Genesis,
            vote.block.slot,
            VotePayload::Block(vote.block),
        ),
    }
}
