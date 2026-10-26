//! Recovers reward votes the leader missed, from verified certificates.
//!
//! The leader of slot S+8 publishes reward certificates for slot S, built from the votes it
//! received directly: the *baseline*. A vote can miss the leader yet reach other validators,
//! whose certificates then contain it. The leader uses those certificates to include such
//! validators. Replay rejects the leader's whole block over one invalid reward certificate, so
//! nothing is published unless it verified exactly as replay verifies it.
//!
//! # Terms
//!
//! - *Seed*: a verified certificate whose signers all signed one ordinary vote, a skip or the
//!   notarization of one block. Notarize, FinalizeFast, primary-only NotarizeFallback and Skip
//!   certificates qualify; mixed primary/fallback certificates cannot be separated.
//! - *Payload*: that vote. Seeds and raw components are kept per payload.
//! - *Raw component*: one validator's signature from an optimistic batch, which verified only as a
//!   whole. Components are therefore untrusted.
//! - *Extension*: a seed plus the raw components of signers outside it, verified as a whole.
//! - *Prepared* candidates: verified extensions, kept for the build.
//! - *Sealed*: a source slot whose reward certificates were built. It is never recreated.
//!
//! # Verification
//!
//! One background worker checks extensions with the `verify_base2` call replay uses, against the
//! source slot's epoch rank map and the local shred version. Colluders can submit components that
//! cancel within their batch, so a failed extension is bisected: parts that verify with the seed
//! are combined and checked again, and isolated invalid components are evicted, since their
//! senders cannot replace them. Seed preparation and extensions share a 27-check job budget. Once
//! seeds are prepared, that is enough to isolate an invalid component among 2,000 voters and still
//! combine the verified parts. Preparation that exhausts the budget schedules another job. It skips
//! extensions that another seed or a prepared extension already covers. Jobs extend seeds, not
//! earlier extensions, so while invalid components remain, each job bisects again from its seed.
//!
//! # Selection
//!
//! A build picks the skip/notar pair with the most unique stake among the baseline, the seeds and
//! the prepared extensions, but only pairs that keep every validator the baseline pays. The
//! exception is a validator whose own component a check proved invalid: its vote only verified in
//! a batch where colluders' errors cancelled, and must not keep honest signers out. Before
//! publishing a certificate beyond the baseline, the worker verifies its exact payload, signature
//! and bitmap as replay will. The immutable checked candidate also pins its source epoch rank map
//! and shred version. Builds select only matching checked candidates and never verify signatures.
//!
//! In production a separate controller owns recovery admission, worker coordination and selection.
//! The baseline service publishes its direct response first, with a bounded channel for an optional
//! upgrade. The requestor takes that upgrade only if it is already available, so slow or saturated
//! recovery cannot withhold the baseline. Each request fixes a cutoff before assembling its direct
//! snapshot: only candidates checked by then are eligible, and only component failures proved by
//! then may relax the baseline's required signer set.
//!
//! # Bounds and deadlines
//!
//! A source slot keeps at most four payloads and four seeds, eight prepared extensions per payload
//! and one raw component per rank. When payloads are full, a batch with more raw stake displaces
//! the weakest payload that holds only raw components; signers with more stake than each batch of
//! a real payload can keep it out until a certificate for it arrives, and the baseline still pays
//! those votes. A payload is checked at most every 62.5 ms, least recently checked first, with one
//! job in flight. Builds never wait for the worker, late inputs and results cannot recreate a
//! sealed slot, and state is dropped when the source slot's reward window closes. If the worker
//! exits, recovery keeps serving its prepared candidates.
//!
//! # Metrics and benchmarks
//!
//! `reward-certs-recovery` reports checks, failures, evictions, fallbacks, late inputs, recovered
//! stake, and worker time. `unverified_outputs` counts admitted
//! seeds rejected by their replay recheck, before publication; it should stay zero because ingress
//! verified those seeds. The `reward_recovery` criterion benches measure builds and jobs at 1,024
//! and 2,000 validators.

use {
    super::super::msg_types::RewardRespSucc,
    agave_bls_cert_verify::cert_verify::verify_base2,
    agave_bls_sigverify::rewards::certificate_reward_vote,
    agave_math_utils::welford_stats::WelfordStats,
    agave_votor_messages::{
        certificate::Certificate,
        consensus_message::Block,
        reward_certificate::{NotarRewardCertificate, SkipRewardCertificate},
        vote::Vote,
        wire::get_vote_payload_to_sign,
    },
    bitvec::vec::BitVec,
    crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded},
    solana_bls_signatures::{
        Signature, SignatureCompressed, SignatureProjective, signature::SignatureAffine,
    },
    solana_clock::Slot,
    solana_runtime::{bank::MAX_ALPENGLOW_VOTE_ACCOUNTS, epoch_stakes::BLSPubkeyToRankMap},
    solana_signer_store::{Decoded, decode, encode_base2},
    std::{
        cmp::Reverse,
        collections::{BTreeMap, BTreeSet, HashMap, btree_map::Entry},
        sync::Arc,
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    },
};

mod service;
pub(in crate::block_creation_loop::rewards) use service::RecoveryService;

// Bound storage and pairing work independently of the number of certificate variants or block IDs
// a peer sends. Updates coalesce while the single worker is busy.
const MAX_CERTIFICATES_PER_SLOT: usize = 4;
const MAX_PAYLOADS_PER_SLOT: usize = 4;
const MAX_READY_CANDIDATES_PER_PAYLOAD: usize = 8;
// Pace work throughout the reward window rather than exhausting a lifetime budget on early
// partial batches. Each job checks at most four seeds, on a single background worker.
const CHECK_INTERVAL: Duration = Duration::from_micros(62_500);
// Bisecting a failed extension isolates an invalid raw component with at most two checks for
// each halving of the largest rank map.
const MAX_BISECTION_CHECKS: usize =
    2 * (usize::BITS - (MAX_ALPENGLOW_VOTE_ACCOUNTS - 1).leading_zeros()) as usize;
// Seed preparation shares this budget with extensions. Once the seeds are prepared, this covers
// every seed's complete extension, one bisection and the narrower extension it leaves. If preparing
// new seeds prevents an exhausted job from isolating a component, it schedules another paced job.
const MAX_CHECKS_PER_JOB: u64 = (MAX_CERTIFICATES_PER_SLOT + MAX_BISECTION_CHECKS + 1) as u64;

#[derive(Clone)]
struct Candidate {
    vote: Vote,
    signature: SignatureCompressed,
    bitmap: Vec<u8>,
    ranks: BitVec<u8>,
    stake: u64,
}

impl Candidate {
    fn same_evidence(&self, other: &Self) -> bool {
        self.vote == other.vote && self.signature == other.signature && self.bitmap == other.bitmap
    }

    fn covers(&self, other: &Self) -> bool {
        self.vote == other.vote && other.ranks.iter_ones().all(|rank| self.ranks[rank])
    }

    fn new(
        vote: Vote,
        signature: SignatureCompressed,
        mut ranks: BitVec<u8>,
        rank_map: &BLSPubkeyToRankMap,
    ) -> Option<Self> {
        if ranks.not_any() || ranks.len() > rank_map.len() {
            return None;
        }
        let stake = ranks.iter_ones().try_fold(0u64, |stake, rank| {
            Some(stake.saturating_add(rank_map.get_pubkey_stake_entry(rank)?.stake.get().get()))
        })?;
        // Selection reads raw bytes. Keep every owned signer set aligned to the first bit of its
        // first byte before encoding as well, since the encoding copies raw bitmap bytes.
        ranks.force_align();
        let bitmap = encode_base2(&ranks).ok()?;
        ranks.resize(rank_map.len(), false);
        Some(Self {
            vote,
            signature,
            bitmap,
            ranks,
            stake,
        })
    }

    fn from_certificate(cert: Certificate, rank_map: &BLSPubkeyToRankMap) -> Option<Self> {
        let vote = certificate_reward_vote(&cert.cert_type, &cert.bitmap, rank_map.len())?;
        let ranks = match decode(&cert.bitmap, rank_map.len()).ok()? {
            Decoded::Base2(ranks) => ranks,
            Decoded::Base3(ranks, fallback) if fallback.not_any() => ranks,
            _ => return None,
        };
        Self::new(vote, cert.signature.try_into().ok()?, ranks, rank_map)
    }
}

/// The context used for replay verification. Pointer identity pins the exact immutable rank map,
/// including its public keys and stakes, rather than merely its length or epoch number.
#[derive(Clone)]
struct CheckContext {
    rank_map: Arc<BLSPubkeyToRankMap>,
    shred_version: u16,
}

impl CheckContext {
    fn matches(&self, rank_map: &Arc<BLSPubkeyToRankMap>, shred_version: u16) -> bool {
        Arc::ptr_eq(&self.rank_map, rank_map) && self.shred_version == shred_version
    }
}

/// Owns exactly the evidence that passed replay verification. Its payload, bitmap, signature and
/// rank metadata are never changed after this value is constructed.
#[derive(Clone)]
struct CheckedCandidate {
    candidate: Candidate,
    context: CheckContext,
    checked_at: Instant,
}

impl CheckedCandidate {
    fn verify(
        candidate: Candidate,
        rank_map: &Arc<BLSPubkeyToRankMap>,
        shred_version: u16,
    ) -> Option<Self> {
        let payload = get_vote_payload_to_sign(candidate.vote, shred_version);
        verifies(&payload, &candidate, rank_map).then(|| Self {
            candidate,
            context: CheckContext {
                rank_map: rank_map.clone(),
                shred_version,
            },
            checked_at: Instant::now(),
        })
    }
}

impl std::ops::Deref for CheckedCandidate {
    type Target = Candidate;

    fn deref(&self) -> &Self::Target {
        &self.candidate
    }
}

#[derive(Clone)]
enum SeedCheck {
    Valid(CheckedCandidate),
    Invalid {
        candidate: Candidate,
        context: CheckContext,
    },
}

impl SeedCheck {
    fn candidate(&self) -> &Candidate {
        match self {
            Self::Valid(candidate) => candidate,
            Self::Invalid { candidate, .. } => candidate,
        }
    }

    fn context(&self) -> &CheckContext {
        match self {
            Self::Valid(candidate) => &candidate.context,
            Self::Invalid { context, .. } => context,
        }
    }
}

/// Verifies `candidate` over `payload` exactly as replay verifies a reward certificate.
fn verifies(payload: &[u8], candidate: &Candidate, rank_map: &BLSPubkeyToRankMap) -> bool {
    verify_base2(
        payload,
        &candidate.signature,
        &candidate.bitmap,
        rank_map.len(),
        |rank| {
            rank_map
                .get_pubkey_stake_entry(rank)
                .map(|entry| entry.bls_pubkey)
        },
        None,
    )
    .is_ok()
}

/// Stake of the validator at `rank`, which comes from a signer set sized by the same rank map.
fn stake_of(rank_map: &BLSPubkeyToRankMap, rank: usize) -> u64 {
    rank_map
        .get_pubkey_stake_entry(rank)
        .expect("signer sets are sized by their rank map")
        .stake
        .get()
        .get()
}

fn stake_of_ranks(rank_map: &BLSPubkeyToRankMap, ranks: impl IntoIterator<Item = usize>) -> u64 {
    ranks.into_iter().fold(0u64, |stake, rank| {
        stake.saturating_add(stake_of(rank_map, rank))
    })
}

#[cfg(test)]
fn union_stake(left: &Candidate, right: &Candidate, rank_map: &BLSPubkeyToRankMap) -> u64 {
    let added = right.ranks.iter_ones().filter(|&rank| !left.ranks[rank]);
    left.stake.saturating_add(stake_of_ranks(rank_map, added))
}

// Prefer evidence that improves the representable skip/notar union, then signer diversity.
// Counting each signer's stake once prevents redundant large certificates crowding out a
// complementary proof. The final tie-break retains stronger standalone fallbacks.
//
// A full admission can score twenty different payload/seed removal plans. Their candidates
// share the same rank map, so compute each expensive pair union once rather than once per plan.
struct CoverageQuality<'a> {
    candidates: Vec<&'a Candidate>,
    pair_stakes: Vec<(usize, usize, u64)>,
    stake_lookup: Option<StakeLookup>,
    rank_map: &'a BLSPubkeyToRankMap,
}

impl<'a> CoverageQuality<'a> {
    fn new(candidates: Vec<&'a Candidate>, rank_map: &'a BLSPubkeyToRankMap) -> Self {
        let mut pair_stakes = vec![];
        // Ready-frontier pruning contains only one vote kind. Avoid building the lookup there;
        // admission with both kinds scores every pair against this one immutable rank map.
        let mut stake_lookup = None;
        for (skip_index, skip) in candidates.iter().enumerate() {
            if !matches!(skip.vote, Vote::Skip(_)) {
                continue;
            }
            for (notar_index, notar) in candidates.iter().enumerate() {
                if matches!(notar.vote, Vote::Notarize(_)) {
                    let stake_lookup =
                        stake_lookup.get_or_insert_with(|| StakeLookup::new(rank_map));
                    pair_stakes.push((
                        skip_index,
                        notar_index,
                        stake_lookup
                            .union_stake(skip.ranks.as_raw_slice(), notar.ranks.as_raw_slice()),
                    ));
                }
            }
        }
        Self {
            candidates,
            pair_stakes,
            stake_lookup,
            rank_map,
        }
    }

    fn score(&self, kept: impl IntoIterator<Item = usize>) -> (u64, u64, u64) {
        let mut included = vec![false; self.candidates.len()];
        let mut best = 0;
        let mut ranks = BitVec::<u8>::repeat(false, self.rank_map.len());
        let mut total = 0u64;
        for index in kept {
            included[index] = true;
            let candidate = self.candidates[index];
            best = best.max(candidate.stake);
            total = total.saturating_add(candidate.stake);
            ranks |= &candidate.ranks;
        }
        for &(skip, notar, stake) in &self.pair_stakes {
            if included[skip] && included[notar] {
                best = best.max(stake);
            }
        }
        let covered = self.stake_lookup.as_ref().map_or_else(
            || stake_of_ranks(self.rank_map, ranks.iter_ones()),
            |lookup| lookup.union_stake(ranks.as_raw_slice(), ranks.as_raw_slice()),
        );
        (best, covered, total)
    }
}

fn retain_ready(
    ready: &mut Vec<CheckedCandidate>,
    candidate: CheckedCandidate,
    rank_map: &BLSPubkeyToRankMap,
) {
    if ready.iter().any(|existing| existing.covers(&candidate)) {
        return;
    }
    ready.retain(|existing| !candidate.covers(existing));
    ready.push(candidate);
    if ready.len() > MAX_READY_CANDIDATES_PER_PAYLOAD {
        // Normally newer generations dominate their older counterparts. Bound the frontier
        // even when certificate replacement introduces additional incomparable signer sets.
        let quality = CoverageQuality::new(ready.iter().map(|c| &c.candidate).collect(), rank_map);
        let remove = (0..ready.len())
            .max_by_key(|&remove| quality.score((0..ready.len()).filter(|&index| index != remove)))
            .expect("the frontier is over its bound, so not empty");
        ready.remove(remove);
    }
}

#[derive(Default)]
struct PayloadState {
    // These are deliberately raw Signature bytes, not VoteAggregate or verified VoteMessage.
    signatures: BTreeMap<u16, Signature>,
    /// Ranks whose component a check against a verified seed proved invalid.
    evicted: BTreeMap<u16, Instant>,
    seeds: Vec<Candidate>,
    /// At most one check per retained seed, including failed checks so bad evidence cannot spin.
    checked_seeds: Vec<SeedCheck>,
    ready: Vec<CheckedCandidate>,
    generation: u64,
    scheduled_generation: u64,
    last_check: Option<Instant>,
    rate_limited_generation: Option<u64>,
}

impl PayloadState {
    /// Stake of the signers of retained raw components.
    fn raw_stake(&self, rank_map: &BLSPubkeyToRankMap) -> u64 {
        stake_of_ranks(
            rank_map,
            self.signatures.keys().map(|&rank| usize::from(rank)),
        )
    }

    fn needs_check(&self) -> bool {
        self.seeds.iter().any(|seed| {
            !self
                .checked_seeds
                .iter()
                .any(|checked| checked.candidate().same_evidence(seed))
        }) || (self.generation != self.scheduled_generation
            && self.checked_seeds.iter().any(|seed| {
                let SeedCheck::Valid(seed) = seed else {
                    return false;
                };
                self.signatures
                    .keys()
                    .any(|&rank| !seed.ranks[usize::from(rank)])
            }))
    }
}

struct SlotState {
    rank_map: Arc<BLSPubkeyToRankMap>,
    payloads: HashMap<Vote, PayloadState>,
    num_certificates: usize,
}

struct Job {
    vote: Vote,
    generation: u64,
    rank_map: Arc<BLSPubkeyToRankMap>,
    shred_version: u16,
    seeds: Vec<Candidate>,
    checked_seeds: Vec<SeedCheck>,
    signatures: BTreeMap<u16, Signature>,
    /// Signer sets of this payload's prepared candidates when the job was scheduled.
    ready: Vec<BitVec<u8>>,
    #[cfg(test)]
    verification_gate: Option<(Sender<()>, Receiver<()>)>,
}

struct JobResult {
    vote: Vote,
    generation: u64,
    candidates: Vec<CheckedCandidate>,
    seed_checks: Vec<SeedCheck>,
    /// Preparing a seed consumed checks needed by an unfinished extension. Preparing the seed is
    /// progress, so the next job can safely retry with the full extension budget.
    retry: bool,
    rejected_seeds: u64,
    /// Raw components shown to be invalid, which later checks must exclude.
    invalid_components: Vec<(u16, Signature, Instant)>,
    checks: u64,
    failed_checks: u64,
    seed_fallbacks: u64,
    worker_us: u64,
}

// A transient worker return value, never a collection: keep successful candidates inline rather
// than allocating a box for every successful signature check.
#[allow(clippy::large_enum_variant)]
enum Check {
    Valid(CheckedCandidate),
    Invalid,
    /// The job's check budget is exhausted.
    Exhausted,
}

/// Extends verified seeds with raw components, verifying each extension as a replayer would.
struct Checker<'a> {
    vote: Vote,
    payload: Vec<u8>,
    rank_map: &'a Arc<BLSPubkeyToRankMap>,
    shred_version: u16,
    /// Raw components, with their points once an extension needs them. Validating a point is the
    /// costly part of aggregation, so each component is validated at most once per job, and only
    /// if an extension that will be checked uses it.
    components: Vec<(u16, &'a Signature, Option<SignatureAffine>)>,
    checks: u64,
    failed_checks: u64,
    exhausted: bool,
}

impl Checker<'_> {
    /// Validates the points of `part` that no earlier extension needed. Returns the components
    /// that are not valid signature points, which no extension can use.
    fn validate(&mut self, part: &[usize]) -> Vec<usize> {
        let mut invalid = vec![];
        for &index in part {
            let (_, signature, point) = &mut self.components[index];
            if point.is_none() {
                match SignatureAffine::try_from(*signature) {
                    Ok(valid) => *point = Some(valid),
                    Err(_) => invalid.push(index),
                }
            }
        }
        invalid
    }

    /// Checks the extension of `seed` with `part`, unless the job has already made `limit` checks.
    fn check(
        &mut self,
        seed: &Candidate,
        seed_signature: &SignatureProjective,
        part: &[usize],
        limit: u64,
    ) -> Check {
        if self.checks >= limit {
            self.exhausted = true;
            return Check::Exhausted;
        }
        let mut signature = *seed_signature;
        let mut ranks = seed.ranks.clone();
        for &index in part {
            ranks.set(usize::from(self.components[index].0), true);
        }
        // Every part comes from an extension whose points were validated.
        debug_assert!(part.iter().all(|&index| self.components[index].2.is_some()));
        if signature
            .aggregate_with(
                part.iter()
                    .filter_map(|&index| self.components[index].2.as_ref()),
            )
            .is_err()
        {
            return Check::Invalid;
        }
        let Some(candidate) = Candidate::new(self.vote, signature.into(), ranks, self.rank_map)
        else {
            return Check::Invalid;
        };
        self.checks += 1;
        if verifies(&self.payload, &candidate, self.rank_map) {
            Check::Valid(CheckedCandidate {
                candidate,
                context: CheckContext {
                    rank_map: self.rank_map.clone(),
                    shred_version: self.shred_version,
                },
                checked_at: Instant::now(),
            })
        } else {
            self.failed_checks += 1;
            Check::Invalid
        }
    }

    /// Splits a failed extension, depth first, until it isolates invalid components. A part that
    /// verifies with the seed sums to its signers' true signatures, so verified parts can be
    /// combined. Returns them with their candidates, and the isolated invalid components. One
    /// check of the job's budget is left for combining the verified parts.
    fn bisect(
        &mut self,
        seed: &Candidate,
        seed_signature: &SignatureProjective,
        failed: &[usize],
    ) -> (Vec<(Vec<usize>, CheckedCandidate)>, Vec<usize>) {
        let mut verified = vec![];
        let mut invalid = vec![];
        let mut failed_parts = vec![failed];
        while let Some(part) = failed_parts.pop() {
            if let [index] = part {
                invalid.push(*index);
                continue;
            }
            let (left, right) = part.split_at(part.len() / 2);
            match self.check(seed, seed_signature, left, MAX_CHECKS_PER_JOB - 1) {
                Check::Exhausted => break,
                // The right half cannot also verify, or the whole part would have verified.
                Check::Valid(candidate) => {
                    verified.push((left.to_vec(), candidate));
                    failed_parts.push(right);
                }
                Check::Invalid => {
                    match self.check(seed, seed_signature, right, MAX_CHECKS_PER_JOB - 1) {
                        Check::Exhausted => break,
                        Check::Valid(candidate) => verified.push((right.to_vec(), candidate)),
                        Check::Invalid => failed_parts.push(right),
                    }
                    failed_parts.push(left);
                }
            }
        }
        (verified, invalid)
    }
}

impl Job {
    fn run(self) -> JobResult {
        let start = Instant::now();
        #[cfg(test)]
        let mut verification_gate = self.verification_gate;
        let component_ranks: Vec<u16> = self.signatures.keys().copied().collect();
        let mut checker = Checker {
            vote: self.vote,
            payload: get_vote_payload_to_sign(self.vote, self.shred_version),
            rank_map: &self.rank_map,
            shred_version: self.shred_version,
            components: self
                .signatures
                .iter()
                .map(|(&rank, signature)| (rank, signature, None))
                .collect(),
            checks: 0,
            failed_checks: 0,
            exhausted: false,
        };
        let mut seeds = vec![];
        let mut seed_checks = vec![];
        let mut rejected_seeds = 0;
        for seed in self.seeds {
            if let Some(checked) = self.checked_seeds.iter().find(|checked| {
                checked.candidate().same_evidence(&seed)
                    && checked
                        .context()
                        .matches(&self.rank_map, self.shred_version)
            }) {
                if let SeedCheck::Valid(candidate) = checked {
                    seeds.push(candidate.clone());
                }
                continue;
            }
            #[cfg(test)]
            if let Some((started, release)) = verification_gate.take() {
                let _ = started.send(());
                let _ = release.recv();
            }
            // There are at most four retained seeds, so their checks always fit the total budget.
            checker.checks += 1;
            let checked =
                match CheckedCandidate::verify(seed.clone(), &self.rank_map, self.shred_version) {
                    Some(candidate) => {
                        seeds.push(candidate.clone());
                        SeedCheck::Valid(candidate)
                    }
                    None => {
                        checker.failed_checks += 1;
                        rejected_seeds += 1;
                        SeedCheck::Invalid {
                            candidate: seed,
                            context: CheckContext {
                                rank_map: self.rank_map.clone(),
                                shred_version: self.shred_version,
                            },
                        }
                    }
                };
            seed_checks.push(checked);
        }
        // Only replay-checked seeds may suppress another extension or be used to judge a raw
        // component invalid. A bad seed must never cause an honest component's eviction.
        let seed_signers: Vec<_> = seeds.iter().map(|seed| seed.ranks.clone()).collect();
        // Keep alternatives: the largest notar candidate need not complement a skip candidate
        // best, and a seed covering invalid raw components can succeed after another seed fails.
        let mut extensions: Vec<_> = seeds
            .into_iter()
            .filter_map(|seed| {
                let mut ranks = seed.ranks.clone();
                for &rank in self.signatures.keys() {
                    ranks.set(usize::from(rank), true);
                }
                let stake = stake_of_ranks(&self.rank_map, ranks.iter_ones());
                (stake > seed.stake).then_some((stake, seed))
            })
            .collect();
        extensions.sort_by_key(|(stake, _)| Reverse(*stake));
        let mut invalid = vec![false; component_ranks.len()];
        let mut invalid_components = vec![];
        let mut candidates: Vec<CheckedCandidate> = vec![];
        let mut seed_fallbacks = 0;
        for (_, seed) in extensions.into_iter().take(MAX_CERTIFICATES_PER_SLOT) {
            let mut outside: Vec<_> = (0..component_ranks.len())
                .filter(|&index| {
                    !invalid[index] && !seed.ranks[usize::from(component_ranks[index])]
                })
                .collect();
            let prospective = |part: &[usize]| {
                let mut signers = seed.ranks.clone();
                for &index in part {
                    signers.set(usize::from(component_ranks[index]), true);
                }
                signers
            };
            // Equal or smaller signer sets cannot improve any skip/notar union, whether those of a
            // seed or prepared by an earlier job or by this one. A previously failed attempt does
            // not suppress a different seed with the same prospective set.
            let prepared = |ranks: BitVec<u8>, candidates: &[CheckedCandidate]| {
                self.ready
                    .iter()
                    .chain(&seed_signers)
                    .chain(candidates.iter().map(|candidate| &candidate.ranks))
                    .any(|covered| ranks.iter_ones().all(|rank| covered[rank]))
            };
            if outside.is_empty() || prepared(prospective(&outside), &candidates) {
                continue;
            }
            // Without a check left, validating this extension's components would be wasted.
            if checker.checks >= MAX_CHECKS_PER_JOB {
                checker.exhausted = true;
                break;
            }
            let undecodable = checker.validate(&outside);
            if !undecodable.is_empty() {
                for index in undecodable {
                    invalid[index] = true;
                    let (rank, signature, _) = checker.components[index];
                    invalid_components.push((rank, *signature, Instant::now()));
                }
                outside.retain(|&index| !invalid[index]);
                if outside.is_empty() || prepared(prospective(&outside), &candidates) {
                    continue;
                }
            }
            let Ok(seed_signature) = SignatureProjective::try_from(&seed.signature) else {
                continue;
            };
            match checker.check(&seed, &seed_signature, &outside, MAX_CHECKS_PER_JOB) {
                Check::Exhausted => break,
                Check::Valid(candidate) => {
                    seed_fallbacks += u64::from(checker.failed_checks != 0);
                    candidates.push(candidate);
                }
                Check::Invalid => {
                    // An optimistic batch proves only its aggregate, so some of its components
                    // can be invalid. Isolate them and keep the verified remainder.
                    let (verified, isolated) = checker.bisect(&seed, &seed_signature, &outside);
                    for index in isolated {
                        invalid[index] = true;
                        let (rank, signature, _) = checker.components[index];
                        invalid_components.push((rank, *signature, Instant::now()));
                    }
                    let valid: Vec<_> = verified
                        .iter()
                        .flat_map(|(part, _)| part.iter().copied())
                        .collect();
                    if valid.is_empty() || prepared(prospective(&valid), &candidates) {
                        continue;
                    }
                    // A single verified part is the narrower extension. Several parts are
                    // combined and verified again before the result can be published; bisection
                    // leaves a check for that. Disjoint verified parts always combine, so the
                    // strongest part, which verified alone, is only a safeguard.
                    let combined = if verified.len() > 1 {
                        checker.check(&seed, &seed_signature, &valid, MAX_CHECKS_PER_JOB)
                    } else {
                        Check::Exhausted
                    };
                    let candidate = match combined {
                        Check::Valid(candidate) => Some(candidate),
                        Check::Invalid | Check::Exhausted => verified
                            .into_iter()
                            .map(|(_, candidate)| candidate)
                            .max_by_key(|candidate| candidate.stake),
                    };
                    if let Some(candidate) = candidate {
                        seed_fallbacks += 1;
                        candidates.push(candidate);
                    }
                }
            }
        }
        JobResult {
            vote: self.vote,
            generation: self.generation,
            candidates,
            retry: checker.exhausted && !seed_checks.is_empty(),
            seed_checks,
            rejected_seeds,
            invalid_components,
            checks: checker.checks,
            failed_checks: checker.failed_checks,
            seed_fallbacks,
            worker_us: start.elapsed().as_micros().try_into().unwrap_or(u64::MAX),
        }
    }
}

pub(in crate::block_creation_loop::rewards) struct Recovery {
    slots: BTreeMap<Slot, SlotState>,
    jobs: Option<Sender<Job>>,
    results: Receiver<JobResult>,
    worker: Option<JoinHandle<()>>,
    in_flight: bool,
    /// The worker only exits early by panicking. A validator's panic hook ends the process
    /// first, so this matters only for embedders without one.
    worker_exited: bool,
    #[cfg(test)]
    verification_gate: Option<(Sender<()>, Receiver<()>)>,
    #[cfg(test)]
    completion_sender: Option<Sender<Slot>>,
    pub(super) metrics: RecoveryMetrics,
}

/// Recovery counters, which the builder drains into its periodic report.
#[derive(Default)]
pub(super) struct RecoveryMetrics {
    pub(super) checks: u64,
    pub(super) completed_jobs: u64,
    pub(super) failed_checks: u64,
    /// Candidates prepared after a failed check, from another seed or from verified parts.
    pub(super) seed_fallbacks: u64,
    pub(super) rate_limited_updates: u64,
    pub(super) replaced_certificates: u64,
    pub(super) dropped_certificates: u64,
    pub(super) dropped_payloads: u64,
    pub(super) invalid_signature_ranks: u64,
    pub(super) invalid_components: u64,
    pub(super) late_results: u64,
    /// Certificates and raw signatures for a source slot that was already built or pruned.
    pub(super) late_certificates: u64,
    pub(super) late_signature_components: u64,
    pub(super) recovered_stake: u64,
    /// Admitted seed certificates rejected by replay verification during preparation. These
    /// indicate a bug because ingress verifies every seed before forwarding it.
    pub(super) unverified_outputs: u64,
    pub(super) worker_exits: u64,
    pub(super) worker_us: WelfordStats,
}

impl RecoveryMetrics {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            checks,
            completed_jobs,
            failed_checks,
            seed_fallbacks,
            rate_limited_updates,
            replaced_certificates,
            dropped_certificates,
            dropped_payloads,
            invalid_signature_ranks,
            invalid_components,
            late_results,
            late_certificates,
            late_signature_components,
            recovered_stake,
            unverified_outputs,
            worker_exits,
            worker_us,
        } = other;
        self.checks += checks;
        self.completed_jobs += completed_jobs;
        self.failed_checks += failed_checks;
        self.seed_fallbacks += seed_fallbacks;
        self.rate_limited_updates += rate_limited_updates;
        self.replaced_certificates += replaced_certificates;
        self.dropped_certificates += dropped_certificates;
        self.dropped_payloads += dropped_payloads;
        self.invalid_signature_ranks += invalid_signature_ranks;
        self.invalid_components += invalid_components;
        self.late_results += late_results;
        self.late_certificates += late_certificates;
        self.late_signature_components += late_signature_components;
        self.recovered_stake += recovered_stake;
        self.unverified_outputs += unverified_outputs;
        self.worker_exits += worker_exits;
        self.worker_us.merge(worker_us);
    }

    pub(super) fn report(&self) {
        datapoint_info!(
            "reward-certs-recovery",
            ("checks", self.checks, i64),
            ("failed_checks", self.failed_checks, i64),
            ("completed_jobs", self.completed_jobs, i64),
            ("seed_fallbacks", self.seed_fallbacks, i64),
            ("rate_limited_updates", self.rate_limited_updates, i64),
            ("replaced_certificates", self.replaced_certificates, i64),
            ("dropped_certificates", self.dropped_certificates, i64),
            ("dropped_payloads", self.dropped_payloads, i64),
            ("invalid_signature_ranks", self.invalid_signature_ranks, i64),
            ("invalid_components", self.invalid_components, i64),
            ("late_results", self.late_results, i64),
            ("late_certificates", self.late_certificates, i64),
            ("late_signature_components", self.late_signature_components, i64),
            ("recovered_stake", self.recovered_stake, i64),
            ("unverified_outputs", self.unverified_outputs, i64),
            ("worker_exits", self.worker_exits, i64),
            ("worker_us_count", self.worker_us.count(), i64),
            ("worker_us_mean", self.worker_us.mean::<u64>(), Option<i64>),
            ("worker_us_max", self.worker_us.maximum::<u64>(), Option<i64>),
        );
    }
}

impl Recovery {
    pub(super) fn new() -> Self {
        let (jobs_sender, jobs_receiver) = bounded::<Job>(1);
        let (results_sender, results_receiver) = bounded(1);
        let worker = thread::Builder::new()
            .name("solConsRewRecov".into())
            .spawn(move || {
                while let Ok(job) = jobs_receiver.recv() {
                    if results_sender.send(job.run()).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn the reward recovery worker");
        Self::from_worker(jobs_sender, results_receiver, worker)
    }

    fn from_worker(
        jobs: Sender<Job>,
        results: Receiver<JobResult>,
        worker: JoinHandle<()>,
    ) -> Self {
        Self {
            slots: BTreeMap::new(),
            jobs: Some(jobs),
            results,
            worker: Some(worker),
            in_flight: false,
            worker_exited: false,
            #[cfg(test)]
            verification_gate: None,
            #[cfg(test)]
            completion_sender: None,
            metrics: RecoveryMetrics::default(),
        }
    }

    /// Keeps serving prepared candidates, which are already verified, without scheduling new
    /// checks or polling for results that cannot arrive.
    fn on_worker_exit(&mut self) {
        if !self.worker_exited {
            error!("reward recovery worker exited; continuing with prepared candidates only");
            self.worker_exited = true;
            self.metrics.worker_exits += 1;
        }
        self.in_flight = false;
        self.jobs = None;
    }

    pub(super) fn retain_from(&mut self, slot: Slot) {
        self.slots = self.slots.split_off(&slot);
    }

    /// Observes completed result collection without blocking the service or growing a test queue.
    #[cfg(test)]
    pub(super) fn observe_completions(&mut self) -> Receiver<Slot> {
        let (sender, receiver) = bounded(64);
        self.completion_sender = Some(sender);
        receiver
    }

    #[cfg(test)]
    fn notify_completion(&self, slot: Slot) {
        if let Some(sender) = &self.completion_sender {
            let _ = sender.try_send(slot);
        }
    }

    /// The state of `slot`, unless it was created with a rank map of a different size. Inputs are
    /// checked against their own rank map, but the state's signer sets have the size of its own.
    fn slot<'a>(
        slots: &'a mut BTreeMap<Slot, SlotState>,
        slot: Slot,
        rank_map: &Arc<BLSPubkeyToRankMap>,
    ) -> Option<&'a mut SlotState> {
        let state = slots.entry(slot).or_insert_with(|| SlotState {
            rank_map: rank_map.clone(),
            payloads: HashMap::new(),
            num_certificates: 0,
        });
        (state.rank_map.len() == rank_map.len()).then_some(state)
    }

    pub(super) fn add_signatures(
        &mut self,
        vote: Vote,
        signatures: impl IntoIterator<Item = (u16, Signature)>,
        rank_map: &Arc<BLSPubkeyToRankMap>,
    ) {
        if !matches!(vote, Vote::Notarize(_) | Vote::Skip(_)) {
            return;
        }
        let Some(slot) = Self::slot(&mut self.slots, vote.slot(), rank_map) else {
            return;
        };
        let signatures: Vec<_> = signatures.into_iter().collect();
        let mut displaced_payloads = 0;
        if !slot.payloads.contains_key(&vote) && slot.payloads.len() >= MAX_PAYLOADS_PER_SLOT {
            // Signers of made-up blocks can each open a payload with their own vote. Let a batch
            // with more raw stake displace the weakest payload that holds only raw components, so
            // colluders cannot keep out a real payload whose batch outweighs theirs.
            let incoming = stake_of_ranks(
                rank_map,
                signatures
                    .iter()
                    .map(|&(rank, _)| usize::from(rank))
                    .filter(|&rank| rank < rank_map.len())
                    .collect::<BTreeSet<_>>(),
            );
            let weakest = slot
                .payloads
                .iter()
                .filter(|(_, state)| state.seeds.is_empty() && state.ready.is_empty())
                .map(|(&vote, state)| (state.raw_stake(rank_map), vote))
                .min_by_key(|&(stake, _)| stake);
            match weakest {
                Some((stake, weakest)) if stake < incoming => {
                    slot.payloads.remove(&weakest);
                    displaced_payloads += 1;
                }
                _ => {
                    self.metrics.dropped_payloads += 1;
                    return;
                }
            }
        }
        let state = slot.payloads.entry(vote).or_default();
        let mut invalid_ranks = 0;
        for (rank, signature) in signatures {
            if usize::from(rank) >= rank_map.len() {
                invalid_ranks += 1;
                continue;
            }
            if let Entry::Vacant(entry) = state.signatures.entry(rank) {
                entry.insert(signature);
                state.generation = state.generation.saturating_add(1);
            }
        }
        self.metrics.invalid_signature_ranks += invalid_ranks;
        self.metrics.dropped_payloads += displaced_payloads;
    }

    pub(super) fn add_certificate(
        &mut self,
        cert: Certificate,
        rank_map: &Arc<BLSPubkeyToRankMap>,
    ) {
        let Some(candidate) = Candidate::from_certificate(cert, rank_map) else {
            return;
        };
        let Some(slot) = Self::slot(&mut self.slots, candidate.vote.slot(), rank_map) else {
            return;
        };
        if slot
            .payloads
            .get(&candidate.vote)
            .is_some_and(|state| state.seeds.iter().any(|seed| seed.ranks == candidate.ranks))
        {
            return;
        }
        let new_payload = !slot.payloads.contains_key(&candidate.vote);
        if slot.num_certificates >= MAX_CERTIFICATES_PER_SLOT
            || (new_payload && slot.payloads.len() >= MAX_PAYLOADS_PER_SLOT)
        {
            let seeds: Vec<_> = slot
                .payloads
                .iter()
                .flat_map(|(&vote, state)| {
                    state
                        .seeds
                        .iter()
                        .enumerate()
                        .map(move |(index, seed)| (vote, index, seed))
                })
                .collect();
            let evidence: Vec<_> = slot
                .payloads
                .iter()
                .flat_map(|(&vote, state)| {
                    state
                        .seeds
                        .iter()
                        .enumerate()
                        .map(move |(index, seed)| (Some((vote, index)), seed))
                        .chain(
                            state
                                .ready
                                .iter()
                                .map(|candidate| (None, &candidate.candidate)),
                        )
                })
                .collect();
            let new_index = evidence.len();
            let quality = CoverageQuality::new(
                evidence
                    .iter()
                    .map(|&(_, candidate)| candidate)
                    .chain(std::iter::once(&candidate))
                    .collect(),
                rank_map,
            );
            let current = quality.score(0..new_index);
            let mut best = None;
            let payload_removals: Vec<_> =
                if new_payload && slot.payloads.len() >= MAX_PAYLOADS_PER_SLOT {
                    slot.payloads.keys().copied().map(Some).collect()
                } else {
                    vec![None]
                };
            for remove_payload in payload_removals {
                // Raw components do not count towards coverage, so among plans with equal
                // coverage, remove the payload with the least raw stake.
                let removed_raw_stake =
                    remove_payload.map_or(0, |vote| slot.payloads[&vote].raw_stake(rank_map));
                for remove_seed in std::iter::once(None)
                    .chain(seeds.iter().map(|&(vote, index, _)| Some((vote, index))))
                {
                    let kept_seeds = seeds
                        .iter()
                        .filter(|&&(vote, index, _)| {
                            Some(vote) != remove_payload && Some((vote, index)) != remove_seed
                        })
                        .count();
                    if kept_seeds >= MAX_CERTIFICATES_PER_SLOT {
                        continue;
                    }
                    let kept = evidence
                        .iter()
                        .enumerate()
                        .filter_map(|(index, &(seed, c))| {
                            (Some(c.vote) != remove_payload
                                && (remove_seed.is_none() || seed != remove_seed))
                                .then_some(index)
                        });
                    let score = quality.score(kept.chain(std::iter::once(new_index)));
                    let key = (score, Reverse(removed_raw_stake));
                    if score > current && best.as_ref().is_none_or(|(best, _)| key > *best) {
                        best = Some((key, (remove_payload, remove_seed)));
                    }
                }
            }
            let Some((_, (remove_payload, remove_seed))) = best else {
                self.metrics.dropped_certificates += 1;
                return;
            };
            let mut removed = 0;
            if let Some(vote) = remove_payload {
                removed += slot
                    .payloads
                    .remove(&vote)
                    .map_or(0, |state| state.seeds.len());
            }
            if let Some((vote, index)) = remove_seed
                && Some(vote) != remove_payload
                && let Some(state) = slot.payloads.get_mut(&vote)
            {
                let removed_seed = state.seeds.remove(index);
                // A broader seed is not publishable until its worker check completes. Preserve
                // the old checked proof as a bounded prepared fallback in the meantime.
                if let Some(checked) =
                    state
                        .checked_seeds
                        .iter()
                        .find_map(|checked| match checked {
                            SeedCheck::Valid(checked)
                                if checked.candidate.same_evidence(&removed_seed) =>
                            {
                                Some(checked.clone())
                            }
                            _ => None,
                        })
                {
                    retain_ready(&mut state.ready, checked, &slot.rank_map);
                }
                state.checked_seeds.retain(|checked| {
                    state
                        .seeds
                        .iter()
                        .any(|seed| checked.candidate().same_evidence(seed))
                });
                state.generation = state.generation.saturating_add(1);
                removed += 1;
            }
            slot.num_certificates -= removed;
            self.metrics.replaced_certificates += removed as u64;
            self.metrics.dropped_payloads += u64::from(remove_payload.is_some());
        }
        let state = slot.payloads.entry(candidate.vote).or_default();
        state.seeds.push(candidate);
        slot.num_certificates += 1;
        state.generation = state.generation.saturating_add(1);
    }

    pub(super) fn collect_ready(&mut self) {
        loop {
            let result = match self.results.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.on_worker_exit();
                    break;
                }
            };
            self.in_flight = false;
            self.metrics.completed_jobs += 1;
            self.metrics.checks += result.checks;
            self.metrics.failed_checks += result.failed_checks;
            self.metrics.seed_fallbacks += result.seed_fallbacks;
            self.metrics.unverified_outputs += result.rejected_seeds;
            self.metrics.worker_us.add_sample(result.worker_us);
            let Some(slot) = self.slots.get_mut(&result.vote.slot()) else {
                self.metrics.late_results += 1;
                #[cfg(test)]
                self.notify_completion(result.vote.slot());
                continue;
            };
            let Some(state) = slot.payloads.get_mut(&result.vote) else {
                self.metrics.late_results += 1;
                #[cfg(test)]
                self.notify_completion(result.vote.slot());
                continue;
            };
            // Slots removed at the cutoff are never recreated. An older generation is still valid
            // evidence, but must not overwrite a newer, better prepared candidate.
            if result.generation <= state.generation {
                for checked in result.seed_checks {
                    if state
                        .seeds
                        .iter()
                        .any(|seed| checked.candidate().same_evidence(seed))
                        && Arc::ptr_eq(&checked.context().rank_map, &slot.rank_map)
                    {
                        state.checked_seeds.retain(|existing| {
                            !existing.candidate().same_evidence(checked.candidate())
                        });
                        state.checked_seeds.push(checked);
                    }
                }
                for candidate in result.candidates {
                    retain_ready(&mut state.ready, candidate, &slot.rank_map);
                }
            }
            // A sender cannot replace its component for a payload, and keeping an invalid one
            // would fail every later extension that includes it.
            let mut evicted = false;
            for (rank, signature, checked_at) in result.invalid_components {
                if state.signatures.get(&rank) == Some(&signature) {
                    state.signatures.remove(&rank);
                    state.evicted.entry(rank).or_insert(checked_at);
                    self.metrics.invalid_components += 1;
                    evicted = true;
                }
            }
            if evicted || result.retry {
                state.generation = state.generation.saturating_add(1);
            }
            #[cfg(test)]
            self.notify_completion(result.vote.slot());
        }
    }

    pub(super) fn poll(&mut self, shred_version: u16) {
        self.collect_ready();
        if self.in_flight || self.worker_exited {
            return;
        }
        let mut next = None;
        for (&slot_number, slot) in &mut self.slots {
            for (&vote, state) in &mut slot.payloads {
                let checked_len = state.checked_seeds.len();
                let ready_len = state.ready.len();
                state
                    .checked_seeds
                    .retain(|checked| checked.context().matches(&slot.rank_map, shred_version));
                state
                    .ready
                    .retain(|candidate| candidate.context.matches(&slot.rank_map, shred_version));
                if checked_len != state.checked_seeds.len() || ready_len != state.ready.len() {
                    state.generation = state.generation.saturating_add(1);
                }
                if !state.needs_check() {
                    // A job already covers this generation, or no seed misses a component. Either
                    // way, later polls need not scan it again.
                    state.scheduled_generation = state.generation;
                    continue;
                }
                if state
                    .last_check
                    .is_some_and(|last| last.elapsed() < CHECK_INTERVAL)
                {
                    if state.rate_limited_generation != Some(state.generation) {
                        self.metrics.rate_limited_updates += 1;
                        state.rate_limited_generation = Some(state.generation);
                    }
                    continue;
                }
                // An old, continuously changing slot must not monopolize the worker. Payloads
                // never checked go first; otherwise schedule the least recently checked payload.
                if next.is_none_or(|(_, _, last)| state.last_check < last) {
                    next = Some((slot_number, vote, state.last_check));
                }
            }
        }
        let Some((slot_number, vote, _)) = next else {
            return;
        };
        let Some(jobs) = &self.jobs else {
            return;
        };
        let Some(slot) = self.slots.get_mut(&slot_number) else {
            return;
        };
        let Some(state) = slot.payloads.get_mut(&vote) else {
            return;
        };
        let job = Job {
            vote,
            generation: state.generation,
            rank_map: slot.rank_map.clone(),
            shred_version,
            seeds: state.seeds.clone(),
            checked_seeds: state.checked_seeds.clone(),
            signatures: state.signatures.clone(),
            ready: state
                .ready
                .iter()
                .map(|candidate| candidate.ranks.clone())
                .collect(),
            #[cfg(test)]
            verification_gate: self.verification_gate.take(),
        };
        match jobs.try_send(job) {
            Ok(()) => {
                state.scheduled_generation = state.generation;
                state.last_check = Some(Instant::now());
                self.in_flight = true;
            }
            Err(TrySendError::Disconnected(_)) => self.on_worker_exit(),
            Err(TrySendError::Full(_)) => (),
        }
    }

    pub(super) fn poll_interval(&self) -> Duration {
        if !self.worker_exited
            && (self.in_flight
                || self
                    .slots
                    .values()
                    .any(|slot| slot.payloads.values().any(PayloadState::needs_check)))
        {
            Duration::from_millis(10)
        } else {
            Duration::from_secs(1)
        }
    }

    pub(super) fn finish(
        &mut self,
        slot: Slot,
        shred_version: u16,
        baseline: RewardRespSucc,
    ) -> RewardRespSucc {
        self.finish_at(slot, shred_version, baseline, Instant::now())
    }

    fn finish_at(
        &mut self,
        slot: Slot,
        shred_version: u16,
        baseline: RewardRespSucc,
        cutoff: Instant,
    ) -> RewardRespSucc {
        self.slots = self.slots.split_off(&slot);
        let Some(state) = self.slots.remove(&slot) else {
            return baseline;
        };
        let mut direct = vec![];
        if let Some(skip) = &baseline.skip {
            direct.push((Vote::new_skip_vote(slot), skip.signature, skip.to_bitmap()));
        }
        if let Some(notar) = &baseline.notar {
            let vote = Vote::new_notarization_vote(Block {
                slot,
                block_id: notar.block_id.into(),
            });
            direct.push((vote, notar.signature, notar.bitmap()));
        }
        // Recovery only adds signers: every validator the direct baseline pays stays paid, unless
        // a check proved its own signature invalid. Such a vote only verified in a batch where
        // colluders' errors cancelled, and must not veto recovery for everyone else.
        let mut required = BitVec::<u8>::repeat(false, state.rank_map.len());
        let mut candidates = vec![];
        for (vote, signature, bitmap) in direct {
            let Ok(Decoded::Base2(ranks)) = decode(bitmap, state.rank_map.len()) else {
                return baseline;
            };
            let Some(candidate) = Candidate::new(vote, signature, ranks, &state.rank_map) else {
                return baseline;
            };
            let mut paid = candidate.ranks.clone();
            if let Some(payload) = state.payloads.get(&vote) {
                for (&rank, &checked_at) in &payload.evicted {
                    if checked_at <= cutoff {
                        paid.set(usize::from(rank), false);
                    }
                }
            }
            required |= &paid;
            candidates.push(candidate);
        }
        for payload in state.payloads.into_values() {
            candidates.extend(
                payload
                    .checked_seeds
                    .into_iter()
                    .filter_map(|seed| match seed {
                        SeedCheck::Valid(checked)
                            if checked.context.matches(&state.rank_map, shred_version)
                                && checked.checked_at <= cutoff =>
                        {
                            Some(checked.candidate)
                        }
                        _ => None,
                    }),
            );
            candidates.extend(payload.ready.into_iter().filter_map(|checked| {
                checked
                    .context
                    .matches(&state.rank_map, shred_version)
                    .then_some(checked)
                    .filter(|checked| checked.checked_at <= cutoff)
                    .map(|checked| checked.candidate)
            }));
        }
        let Some(selected) = select_pair(&candidates, &state.rank_map, &required) else {
            return baseline;
        };
        let baseline_validators: std::collections::HashSet<_> =
            baseline.validators.iter().copied().collect();
        let response = reward_response(slot, selected, &state.rank_map).unwrap_or(baseline);
        self.metrics.recovered_stake += response
            .validators
            .iter()
            .filter(|key| !baseline_validators.contains(key))
            .filter_map(|key| state.rank_map.get_rank_for_vote_pubkey(key))
            .filter_map(|&rank| state.rank_map.get_pubkey_stake_entry(usize::from(rank)))
            .fold(0u64, |stake, entry| {
                stake.saturating_add(entry.stake.get().get())
            });
        response
    }
}

/// The skip/notar pair with the most unique stake among those whose signers include every
/// `required` rank, with the union of its signers.
type Selection<'a> = (Option<&'a Candidate>, Option<&'a Candidate>, BitVec<u8>);

/// Exact weighted stake for every four-rank pattern, derived once from this selection's rank map.
/// At 2,000 ranks the table occupies about 64 KiB; scoring a pair uses two lookups per bitmap byte
/// rather than looking up every included rank. Weights outside the final partial byte are zero.
struct StakeLookup {
    bytes: Vec<[[u64; 16]; 2]>,
}

impl StakeLookup {
    fn new(rank_map: &BLSPubkeyToRankMap) -> Self {
        let bytes = (0..rank_map.len().div_ceil(8))
            .map(|byte| {
                std::array::from_fn(|half| {
                    let stakes: [u64; 4] = std::array::from_fn(|bit| {
                        let rank = byte * 8 + half * 4 + bit;
                        if rank < rank_map.len() {
                            stake_of(rank_map, rank)
                        } else {
                            0
                        }
                    });
                    let mut patterns = [0u64; 16];
                    for pattern in 1usize..16 {
                        let bit = pattern.trailing_zeros() as usize;
                        patterns[pattern] =
                            patterns[pattern & (pattern - 1)].saturating_add(stakes[bit]);
                    }
                    patterns
                })
            })
            .collect();
        Self { bytes }
    }

    fn union_stake(&self, left: &[u8], right: &[u8]) -> u64 {
        left.iter()
            .zip(right)
            .zip(&self.bytes)
            .fold(0u64, |stake, ((left, right), [low, high])| {
                let union = left | right;
                stake
                    .saturating_add(low[usize::from(union & 0x0f)])
                    .saturating_add(high[usize::from(union >> 4)])
            })
    }
}

fn select_pair<'a>(
    candidates: &'a [Candidate],
    rank_map: &BLSPubkeyToRankMap,
    required: &BitVec<u8>,
) -> Option<Selection<'a>> {
    let skips: Vec<_> = std::iter::once(None)
        .chain(
            candidates
                .iter()
                .filter(|c| matches!(c.vote, Vote::Skip(_)))
                .map(Some),
        )
        .collect();
    let notars: Vec<_> = std::iter::once(None)
        .chain(
            candidates
                .iter()
                .filter(|c| matches!(c.vote, Vote::Notarize(_)))
                .map(Some),
        )
        .collect();
    let empty = vec![0; rank_map.len().div_ceil(8)];
    // Both inputs are ordinary owned BitVecs after Candidate::new. Normalize required as well,
    // and exclude dead tail bits: they are not part of the logical signer set.
    let mut required = required.clone();
    required.force_align();
    let required_bytes = required.as_raw_slice();
    let tail_mask = match required.len() % 8 {
        0 => u8::MAX,
        bits => (1u8 << bits) - 1,
    };
    let required: Vec<_> = required_bytes
        .iter()
        .enumerate()
        .filter_map(|(index, &byte)| {
            let byte = if index + 1 == required_bytes.len() {
                byte & tail_mask
            } else {
                byte
            };
            (byte != 0).then_some((index, byte))
        })
        .collect();
    let stake_lookup = (skips.len() > 1 && notars.len() > 1).then(|| StakeLookup::new(rank_map));
    let mut best = None;
    let mut best_stake = 0;
    for skip in skips {
        let skip_bytes = skip.map_or(empty.as_slice(), |skip| skip.ranks.as_raw_slice());
        for &notar in &notars {
            let notar_bytes = notar.map_or(empty.as_slice(), |notar| notar.ranks.as_raw_slice());
            if required
                .iter()
                .any(|&(index, required)| required & !(skip_bytes[index] | notar_bytes[index]) != 0)
            {
                continue;
            }
            let stake = match (skip, notar) {
                (None, None) => 0,
                (Some(candidate), None) | (None, Some(candidate)) => candidate.stake,
                (Some(_), Some(_)) => stake_lookup
                    .as_ref()
                    .expect("both candidate kinds have a stake lookup")
                    .union_stake(skip_bytes, notar_bytes),
            };
            if stake <= best_stake {
                continue;
            }
            best_stake = stake;
            best = Some((skip, notar));
        }
    }
    let (skip, notar) = best?;
    // Allocate and materialize only the selected union, preserving the original first-wins order.
    let mut ranks = BitVec::<u8>::repeat(false, rank_map.len());
    if let Some(skip) = skip {
        ranks |= &skip.ranks;
    }
    if let Some(notar) = notar {
        ranks |= &notar.ranks;
    }
    Some((skip, notar, ranks))
}

fn reward_response(
    slot: Slot,
    (skip, notar, ranks): Selection,
    rank_map: &BLSPubkeyToRankMap,
) -> Option<RewardRespSucc> {
    let skip = skip
        .map(|c| SkipRewardCertificate::try_new(slot, c.signature, c.bitmap.clone()))
        .transpose()
        .ok()?;
    let notar = notar
        .map(|c| {
            let Vote::Notarize(notar) = c.vote else {
                unreachable!("notar candidates carry notarize votes");
            };
            NotarRewardCertificate::try_new(
                slot,
                notar.block.block_id.to_hash(),
                c.signature,
                c.bitmap.clone(),
            )
        })
        .transpose()
        .ok()?;
    Some(RewardRespSucc {
        skip,
        notar,
        validators: ranks
            .iter_ones()
            .map(|rank| {
                rank_map
                    .get_pubkey_stake_entry(rank)
                    .expect("signer sets are sized by their rank map")
                    .vote_account_pubkey
            })
            .collect(),
    })
}

/// Tests build trusted direct baselines with the same selector.
#[cfg(test)]
fn select_candidates(
    slot: Slot,
    candidates: Vec<Candidate>,
    rank_map: &BLSPubkeyToRankMap,
    required: &BitVec<u8>,
) -> Option<RewardRespSucc> {
    reward_response(
        slot,
        select_pair(&candidates, rank_map, required)?,
        rank_map,
    )
}

impl Drop for Recovery {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests;
