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
//! - *Payload*: that vote. Seeds and verified vote aggregates are kept per payload.
//! - *Extension*: a seed plus complete vote aggregates with disjoint signer sets.
//! - *Prepared* candidates: replay-verified extensions, kept for the build.
//! - *Sealed*: a source slot whose reward certificates were built. It is never recreated.
//!
//! # Verification
//!
//! One background worker checks extensions with the `verify_base2` call replay uses, against the
//! source slot's epoch rank map and the local shred version. An optimistic vote batch verifies
//! only as a whole, so recovery keeps its aggregate signature and signer bitmap together. An
//! aggregate that overlaps a seed or an already included aggregate cannot be added or split.
//! Each job checks at most four seeds and one extension per seed. It skips extensions that another
//! seed or a prepared extension already covers.
//!
//! # Selection
//!
//! A build picks the skip/notar pair with the most unique stake among the baseline, the seeds and
//! the prepared extensions, but only pairs that keep every validator the baseline pays. Before
//! publishing a certificate beyond the baseline, the worker verifies its exact payload, signature
//! and bitmap as replay will. The immutable checked candidate also pins its source epoch rank map
//! and shred version. Builds select only matching checked candidates and never verify signatures.
//!
//! In production a separate controller owns recovery admission, worker coordination and selection.
//! The baseline service publishes its direct response first, with a bounded channel for an optional
//! upgrade. The requestor takes that upgrade only if it is already available, so slow or saturated
//! recovery cannot withhold the baseline. Each request fixes a cutoff before assembling its direct
//! snapshot: only candidates checked by then are eligible.
//!
//! # Bounds and deadlines
//!
//! A source slot keeps at most four payloads and four seeds, eight prepared extensions per payload
//! and at most as many vote aggregates per payload as validators in the rank map. When payloads
//! are full, a batch with more stake displaces the weakest payload that holds only direct votes.
//! Signers with more stake than each batch of a real payload can keep it out until a certificate
//! for it arrives, and the baseline still pays those votes. A payload is checked at most every
//! 62.5 ms, least recently checked first, with one job in flight. Builds never wait for the worker,
//! late inputs and results cannot recreate a
//! sealed slot, and state is dropped when the source slot's reward window closes. If the worker
//! exits, recovery keeps serving its prepared candidates.
//!
//! # Metrics and benchmarks
//!
//! `reward-certs-recovery` reports checks, failures, rejected evidence, late inputs, recovered
//! stake, and worker time. `unverified_outputs` counts admitted
//! seeds rejected by their replay recheck, before publication; it should stay zero because ingress
//! verified those seeds. The `reward_recovery` criterion benches measure builds and jobs at 1,024
//! and 2,000 validators.

use {
    super::super::msg_types::RewardRespSucc,
    agave_bls_cert_verify::cert_verify::verify_base2,
    agave_bls_sigverify::{
        rewards::decode_certificate_reward, sig_verified_messages::VoteAggregate,
    },
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
    solana_bls_signatures::{SignatureCompressed, SignatureProjective},
    solana_clock::Slot,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    solana_signer_store::{Decoded, decode, encode_base2},
    std::{
        cmp::Reverse,
        collections::{BTreeMap, HashMap},
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
// Each job checks at most four seeds and one extension of each seed.
const MAX_CHECKS_PER_JOB: u64 = (2 * MAX_CERTIFICATES_PER_SLOT) as u64;

#[derive(Clone)]
struct Candidate {
    vote: Vote,
    signature: SignatureCompressed,
    bitmap: Vec<u8>,
    ranks: BitVec<u8>,
    stake: u64,
}

impl Candidate {
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
        let (vote, ranks) =
            decode_certificate_reward(&cert.cert_type, &cert.bitmap, rank_map.len())?;
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
    candidate: Arc<Candidate>,
    context: CheckContext,
    checked_at: Instant,
}

impl CheckedCandidate {
    fn verify(
        candidate: Arc<Candidate>,
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
    Pending,
    Valid {
        context: CheckContext,
        checked_at: Instant,
    },
    Invalid {
        context: CheckContext,
    },
}

impl SeedCheck {
    fn context(&self) -> Option<&CheckContext> {
        match self {
            Self::Pending => None,
            Self::Valid { context, .. } | Self::Invalid { context } => Some(context),
        }
    }
}

/// Retains one immutable proof with its check status. Worker snapshots share the proof, and a
/// result can update only the same retained seed, even if replacements happened during its check.
#[derive(Clone)]
struct Seed {
    candidate: Arc<Candidate>,
    check: SeedCheck,
}

impl Seed {
    fn new(candidate: Candidate) -> Self {
        Self {
            candidate: Arc::new(candidate),
            check: SeedCheck::Pending,
        }
    }

    fn checked(&self) -> Option<CheckedCandidate> {
        let SeedCheck::Valid {
            context,
            checked_at,
        } = &self.check
        else {
            return None;
        };
        Some(CheckedCandidate {
            candidate: self.candidate.clone(),
            context: context.clone(),
            checked_at: *checked_at,
        })
    }
}

impl std::ops::Deref for Seed {
    type Target = Candidate;

    fn deref(&self) -> &Self::Target {
        &self.candidate
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
        let quality = CoverageQuality::new(
            ready.iter().map(|c| c.candidate.as_ref()).collect(),
            rank_map,
        );
        let remove = (0..ready.len())
            .max_by_key(|&remove| quality.score((0..ready.len()).filter(|&index| index != remove)))
            .expect("the frontier is over its bound, so not empty");
        ready.remove(remove);
    }
}

#[derive(Default)]
struct PayloadState {
    // Keep the signature and signer bitmap together. An optimistic aggregate cannot be split.
    aggregates: Vec<Arc<VoteAggregate>>,
    // Failed checks stay with their seed so bad evidence cannot spin.
    seeds: Vec<Seed>,
    ready: Vec<CheckedCandidate>,
    generation: u64,
    scheduled_generation: u64,
    last_check: Option<Instant>,
    rate_limited_generation: Option<u64>,
}

impl PayloadState {
    /// Unique stake of the signers of retained vote aggregates.
    fn aggregate_stake(&self, rank_map: &BLSPubkeyToRankMap) -> u64 {
        let mut ranks = BitVec::<u8>::repeat(false, rank_map.len());
        for aggregate in &self.aggregates {
            ranks |= aggregate.ranks();
        }
        stake_of_ranks(rank_map, ranks.iter_ones())
    }

    fn needs_check(&self) -> bool {
        self.seeds.iter().any(|seed| {
            matches!(seed.check, SeedCheck::Pending)
                || (self.generation != self.scheduled_generation
                    && matches!(seed.check, SeedCheck::Valid { .. })
                    && self.aggregates.iter().any(|aggregate| {
                        aggregate.ranks().iter_ones().all(|rank| !seed.ranks[rank])
                    }))
        })
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
    seeds: Vec<Seed>,
    aggregates: Vec<Arc<VoteAggregate>>,
    /// Immutable prepared evidence shared with the controller when the job was scheduled.
    ready: Vec<Arc<Candidate>>,
    #[cfg(test)]
    verification_gate: Option<(Sender<()>, Receiver<()>)>,
}

struct JobResult {
    vote: Vote,
    generation: u64,
    candidates: Vec<CheckedCandidate>,
    seeds: Vec<Seed>,
    rejected_seeds: u64,
    checks: u64,
    failed_checks: u64,
    worker_us: u64,
}

impl Job {
    fn run(self) -> JobResult {
        let start = Instant::now();
        #[cfg(test)]
        let mut verification_gate = self.verification_gate;
        let mut checks = 0;
        let mut failed_checks = 0;
        let mut seeds = self.seeds;
        let mut rejected_seeds = 0;
        for seed in &mut seeds {
            if seed
                .check
                .context()
                .is_some_and(|context| context.matches(&self.rank_map, self.shred_version))
            {
                continue;
            }
            #[cfg(test)]
            if let Some((started, release)) = verification_gate.take() {
                let _ = started.send(());
                let _ = release.recv();
            }
            // There are at most four retained seeds, so their checks always fit the total budget.
            checks += 1;
            seed.check = match CheckedCandidate::verify(
                seed.candidate.clone(),
                &self.rank_map,
                self.shred_version,
            ) {
                Some(candidate) => SeedCheck::Valid {
                    context: candidate.context,
                    checked_at: candidate.checked_at,
                },
                None => {
                    failed_checks += 1;
                    rejected_seeds += 1;
                    SeedCheck::Invalid {
                        context: CheckContext {
                            rank_map: self.rank_map.clone(),
                            shred_version: self.shred_version,
                        },
                    }
                }
            };
        }
        // Combine only complete, disjoint aggregates. A certificate that overlaps part of an
        // optimistic batch cannot use the remainder, even when individual signatures cancel.
        let checked_seeds: Vec<_> = seeds.iter().filter_map(Seed::checked).collect();
        let mut aggregates: Vec<_> = self.aggregates.iter().collect();
        aggregates.sort_by_key(|aggregate| Reverse(aggregate.stake()));
        let mut candidates: Vec<CheckedCandidate> = vec![];
        for seed in &checked_seeds {
            let mut ranks = seed.ranks.clone();
            let mut outside = vec![];
            for aggregate in &aggregates {
                if aggregate.ranks().iter_ones().all(|rank| !ranks[rank]) {
                    ranks |= aggregate.ranks();
                    outside.push(aggregate.signature());
                }
            }
            if outside.is_empty()
                || self
                    .ready
                    .iter()
                    .map(|candidate| &candidate.ranks)
                    .chain(checked_seeds.iter().map(|seed| &seed.ranks))
                    .chain(candidates.iter().map(|candidate| &candidate.ranks))
                    .any(|covered| ranks.iter_ones().all(|rank| covered[rank]))
            {
                continue;
            }
            let Ok(mut signature) = SignatureProjective::try_from(&seed.signature) else {
                continue;
            };
            if signature.aggregate_with(outside.into_iter()).is_err() {
                continue;
            }
            let Some(candidate) =
                Candidate::new(self.vote, signature.into(), ranks, &self.rank_map)
            else {
                continue;
            };
            checks += 1;
            match CheckedCandidate::verify(candidate.into(), &self.rank_map, self.shred_version) {
                Some(candidate) => candidates.push(candidate),
                None => failed_checks += 1,
            }
        }
        debug_assert!(checks <= MAX_CHECKS_PER_JOB);
        JobResult {
            vote: self.vote,
            generation: self.generation,
            candidates,
            seeds,
            rejected_seeds,
            checks,
            failed_checks,
            worker_us: start.elapsed().as_micros().try_into().unwrap_or(u64::MAX),
        }
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
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

/// Recovery counters, reported by the controller that owns the engine.
#[derive(Default)]
pub(super) struct RecoveryMetrics {
    pub(super) checks: u64,
    pub(super) completed_jobs: u64,
    pub(super) failed_checks: u64,
    pub(super) rate_limited_updates: u64,
    pub(super) replaced_certificates: u64,
    pub(super) dropped_certificates: u64,
    pub(super) dropped_payloads: u64,
    pub(super) invalid_vote_aggregates: u64,
    pub(super) late_results: u64,
    /// Certificates and vote aggregates for a source slot that was already built or pruned.
    pub(super) late_certificates: u64,
    pub(super) late_vote_aggregates: u64,
    pub(super) recovered_stake: u64,
    /// Admitted seed certificates rejected by replay verification during preparation. These
    /// indicate a bug because ingress verifies every seed before forwarding it.
    pub(super) unverified_outputs: u64,
    pub(super) worker_exits: u64,
    pub(super) worker_us: WelfordStats,
}

impl RecoveryMetrics {
    pub(super) fn report(&self) {
        datapoint_info!(
            "reward-certs-recovery",
            ("checks", self.checks, i64),
            ("failed_checks", self.failed_checks, i64),
            ("completed_jobs", self.completed_jobs, i64),
            ("rate_limited_updates", self.rate_limited_updates, i64),
            ("replaced_certificates", self.replaced_certificates, i64),
            ("dropped_certificates", self.dropped_certificates, i64),
            ("dropped_payloads", self.dropped_payloads, i64),
            ("invalid_vote_aggregates", self.invalid_vote_aggregates, i64),
            ("late_results", self.late_results, i64),
            ("late_certificates", self.late_certificates, i64),
            ("late_vote_aggregates", self.late_vote_aggregates, i64),
            ("recovered_stake", self.recovered_stake, i64),
            ("unverified_outputs", self.unverified_outputs, i64),
            ("worker_exits", self.worker_exits, i64),
            ("worker_us_count", self.worker_us.count(), i64),
            ("worker_us_mean", self.worker_us.mean::<u64>(), Option<i64>),
            ("worker_us_max", self.worker_us.maximum::<u64>(), Option<i64>),
        );
    }
}

impl Default for Recovery {
    fn default() -> Self {
        Self::new()
    }
}

impl Recovery {
    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
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
    pub(in crate::block_creation_loop::rewards) fn observe_completions(
        &mut self,
    ) -> Receiver<Slot> {
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

    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    pub(super) fn add_aggregate(
        &mut self,
        aggregate: VoteAggregate,
        rank_map: &Arc<BLSPubkeyToRankMap>,
    ) {
        let vote = *aggregate.vote();
        if !matches!(vote, Vote::Notarize(_) | Vote::Skip(_)) {
            return;
        }
        if aggregate.ranks().len() != rank_map.len() || aggregate.ranks().not_any() {
            self.metrics.invalid_vote_aggregates += 1;
            return;
        }
        let Some(slot) = Self::slot(&mut self.slots, vote.slot(), rank_map) else {
            return;
        };
        if !slot.payloads.contains_key(&vote) && slot.payloads.len() >= MAX_PAYLOADS_PER_SLOT {
            // A stronger batch may displace a payload that holds only direct votes. Certificate
            // evidence keeps priority, so made-up blocks cannot crowd it out.
            let incoming = stake_of_ranks(rank_map, aggregate.ranks().iter_ones());
            let weakest = slot
                .payloads
                .iter()
                .filter(|(_, state)| state.seeds.is_empty() && state.ready.is_empty())
                .map(|(&vote, state)| (state.aggregate_stake(rank_map), vote))
                .min_by_key(|&(stake, _)| stake);
            match weakest {
                Some((stake, weakest)) if stake < incoming => {
                    slot.payloads.remove(&weakest);
                    self.metrics.dropped_payloads += 1;
                }
                _ => {
                    self.metrics.dropped_payloads += 1;
                    return;
                }
            }
        }
        let state = slot.payloads.entry(vote).or_default();
        // Different overlapping batches can extend different seeds. Keep those alternatives,
        // but bound their count and ignore duplicate signer sets (verified BLS signatures are unique).
        if state.aggregates.len() < rank_map.len()
            && !state
                .aggregates
                .iter()
                .any(|existing| existing.ranks() == aggregate.ranks())
        {
            state.aggregates.push(Arc::new(aggregate));
            state.generation = state.generation.saturating_add(1);
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
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
                        .map(move |(index, seed)| (Some((vote, index)), seed.candidate.as_ref()))
                        .chain(
                            state
                                .ready
                                .iter()
                                .map(|candidate| (None, candidate.candidate.as_ref())),
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
                // Direct vote aggregates do not count towards checked coverage. Among plans with
                // equal coverage, remove the payload with the least direct stake.
                let removed_vote_stake =
                    remove_payload.map_or(0, |vote| slot.payloads[&vote].aggregate_stake(rank_map));
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
                    let key = (score, Reverse(removed_vote_stake));
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
                if let Some(checked) = removed_seed.checked() {
                    retain_ready(&mut state.ready, checked, &slot.rank_map);
                }
                state.generation = state.generation.saturating_add(1);
                removed += 1;
            }
            slot.num_certificates -= removed;
            self.metrics.replaced_certificates += removed as u64;
            self.metrics.dropped_payloads += u64::from(remove_payload.is_some());
        }
        let state = slot.payloads.entry(candidate.vote).or_default();
        state.seeds.push(Seed::new(candidate));
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
                for checked in result.seeds {
                    if checked
                        .check
                        .context()
                        .is_some_and(|context| Arc::ptr_eq(&context.rank_map, &slot.rank_map))
                        && let Some(seed) = state
                            .seeds
                            .iter_mut()
                            .find(|seed| Arc::ptr_eq(&seed.candidate, &checked.candidate))
                    {
                        seed.check = checked.check;
                    }
                }
                for candidate in result.candidates {
                    retain_ready(&mut state.ready, candidate, &slot.rank_map);
                }
            }
            #[cfg(test)]
            self.notify_completion(result.vote.slot());
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    pub(super) fn poll(&mut self, shred_version: u16) {
        self.collect_ready();
        if self.in_flight || self.worker_exited {
            return;
        }
        let mut next = None;
        for (&slot_number, slot) in &mut self.slots {
            for (&vote, state) in &mut slot.payloads {
                let mut invalidated = false;
                for seed in &mut state.seeds {
                    if seed
                        .check
                        .context()
                        .is_some_and(|context| !context.matches(&slot.rank_map, shred_version))
                    {
                        seed.check = SeedCheck::Pending;
                        invalidated = true;
                    }
                }
                let ready_len = state.ready.len();
                state
                    .ready
                    .retain(|candidate| candidate.context.matches(&slot.rank_map, shred_version));
                if invalidated || ready_len != state.ready.len() {
                    state.generation = state.generation.saturating_add(1);
                }
                if !state.needs_check() {
                    // A job already covers this generation, or no seed has a disjoint aggregate. Either
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
            aggregates: state.aggregates.clone(),
            ready: state
                .ready
                .iter()
                .map(|candidate| candidate.candidate.clone())
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

    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
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

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
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
                block_id: notar.block_id,
            });
            direct.push((vote, notar.signature, notar.bitmap()));
        }
        // Recovery only adds signers: every validator the direct baseline pays stays paid.
        let mut required = BitVec::<u8>::repeat(false, state.rank_map.len());
        let mut candidates = vec![];
        for (vote, signature, bitmap) in direct {
            let Ok(Decoded::Base2(ranks)) = decode(bitmap, state.rank_map.len()) else {
                return baseline;
            };
            let Some(candidate) = Candidate::new(vote, signature, ranks, &state.rank_map) else {
                return baseline;
            };
            required |= &candidate.ranks;
            candidates.push(Arc::new(candidate));
        }
        for payload in state.payloads.into_values() {
            candidates.extend(
                payload
                    .seeds
                    .into_iter()
                    .filter_map(|seed| seed.checked())
                    .filter(|checked| {
                        checked.context.matches(&state.rank_map, shred_version)
                            && checked.checked_at <= cutoff
                    })
                    .map(|checked| checked.candidate),
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
        let Some(selected) = select_pair(
            candidates.iter().map(Arc::as_ref),
            &state.rank_map,
            &required,
        ) else {
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
    candidates: impl IntoIterator<Item = &'a Candidate>,
    rank_map: &BLSPubkeyToRankMap,
    required: &BitVec<u8>,
) -> Option<Selection<'a>> {
    let mut skips = vec![None];
    let mut notars = vec![None];
    for candidate in candidates {
        match candidate.vote {
            Vote::Skip(_) => skips.push(Some(candidate)),
            Vote::Notarize(_) => notars.push(Some(candidate)),
            _ => (),
        }
    }
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
                notar.block.block_id,
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
