//! Verifies consensus certificates and, with reward recovery enabled, reward evidence.
//!
//! The consensus path is unchanged: candidates of each certificate type are verified until the
//! first valid one, which is sent to the consensus pool and remembered.
//!
//! Certificates that can carry reward signers are handed to a dedicated reward verifier thread
//! after the pool send, through a nonblocking queue in which unverified candidates may use only
//! half of the space, so consensus never waits for optional work. Block footer certificates
//! arrive the same way, from blockstore. The reward verifier forwards certificates that verified
//! to rewards, and retains up to four signer sets per type. It verifies an unverified candidate
//! only if its signer set is not inside a retained one and, once four are retained, only if it
//! would improve their coverage. It does so within eight attempts per type and eight checks
//! overall per 250 ms, for at most 1,024 types that never verified. Those budgets are not fair
//! across types.
//!
//! Before a candidate uses a batch's limit of eight per type, the consensus loop drops copies of
//! retained signer sets, which it reads without waiting for their lock, and exact repeats within
//! the batch. A forged copy cannot shadow the genuine certificate, but junk and re-encoded copies
//! can still fill the limit, and a peer can keep the reward verifier busy with candidates that add
//! nothing. Reward-only certificates, such as alternative signer sets or certificates below the
//! root, never reach the consensus pool.

use {
    crate::{
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY},
        errors::SigVerifyCertError,
        rewards::{
            RewardCertificateSender, RewardInput, certificate_reward_vote, reward_window_open,
        },
        sig_verified_messages::SigVerifiedBatch,
        stats::{RewardCertVerifierStats, SigVerifyCertStats},
        utils::send_certs_to_pool,
    },
    agave_bls_cert_verify::cert_verify::Error as BlsCertVerifyError,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        unverified_vote_message::UnverifiedCertificate,
    },
    agave_votor_transport::endpoint::BanSender,
    bitvec::vec::BitVec,
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded},
    log::{error, info},
    rayon::{
        ThreadPool,
        iter::{IntoParallelIterator, ParallelIterator},
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    solana_signer_store::{Decoded, decode},
    std::{
        collections::{HashMap, HashSet},
        sync::{Arc, PoisonError, RwLock, TryLockError},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    },
};

/// Keep alternative signer sets bounded even when peers advertise many valid
/// certificates for the same consensus decision.
const MAX_REWARD_CERTIFICATES_PER_TYPE: usize = 4;
/// Bound additional reward candidates once consensus already has a certificate,
/// or when an old certificate is needed only for rewards.
pub(super) const MAX_CERTIFICATE_CANDIDATES_PER_TYPE: usize = 8;
/// Optional reward verification has its own thread, but its total work must
/// remain bounded even when a peer advertises many certificate types.
const MAX_REWARD_VERIFICATIONS_PER_INTERVAL: usize = 8;
const REWARD_VERIFICATION_BUDGET_INTERVAL: Duration = Duration::from_millis(250);
const MAX_REWARD_ONLY_BUDGET_STATES: usize = 1_024;
/// Reward work waiting for the reward verifier. The consensus loop never waits for it.
const MAX_PENDING_REWARD_INPUTS: usize = 256;
/// Unverified candidates are cheap to send in bulk, so they may fill only half of the queue. The
/// rest stays available for consensus proofs, which are the most useful reward evidence.
const MAX_PENDING_REWARD_CANDIDATES: usize = MAX_PENDING_REWARD_INPUTS / 2;

#[derive(Clone)]
struct VerifiedCertificateState {
    bitmaps: Vec<Vec<u8>>,
    remaining_reward_attempts: usize,
    /// Whether a certificate of this type has verified, on either path.
    has_verified_certificate: bool,
    budget_started: Instant,
}

impl VerifiedCertificateState {
    fn new() -> Self {
        Self {
            bitmaps: Vec::new(),
            remaining_reward_attempts: MAX_CERTIFICATE_CANDIDATES_PER_TYPE,
            has_verified_certificate: false,
            budget_started: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        if now.saturating_duration_since(self.budget_started) >= REWARD_VERIFICATION_BUDGET_INTERVAL
        {
            self.remaining_reward_attempts = MAX_CERTIFICATE_CANDIDATES_PER_TYPE;
            self.budget_started = now;
        }
    }
}

/// Retained reward signer sets and verification budgets by certificate type.
struct VerifiedCertificates {
    states: HashMap<CertificateType, VerifiedCertificateState>,
    reward_only_budget_states: usize,
    remaining_reward_attempts: usize,
    reward_budget_started: Instant,
}

impl VerifiedCertificates {
    fn new() -> Self {
        Self {
            states: HashMap::new(),
            reward_only_budget_states: 0,
            remaining_reward_attempts: MAX_REWARD_VERIFICATIONS_PER_INTERVAL,
            reward_budget_started: Instant::now(),
        }
    }

    /// Restores the shared verification budget once its interval has passed.
    fn refill_reward_budget(&mut self, now: Instant) {
        if now.saturating_duration_since(self.reward_budget_started)
            >= REWARD_VERIFICATION_BUDGET_INTERVAL
        {
            self.remaining_reward_attempts = MAX_REWARD_VERIFICATIONS_PER_INTERVAL;
            self.reward_budget_started = now;
        }
    }

    fn get(&self, cert_type: &CertificateType) -> Option<&VerifiedCertificateState> {
        self.states.get(cert_type)
    }

    fn contains_key(&self, cert_type: &CertificateType) -> bool {
        self.states.contains_key(cert_type)
    }

    fn can_admit_reward_only(&self, cert_type: &CertificateType) -> bool {
        self.contains_key(cert_type)
            || self.reward_only_budget_states < MAX_REWARD_ONLY_BUDGET_STATES
    }

    fn insert(&mut self, cert_type: CertificateType, state: VerifiedCertificateState) -> bool {
        if !state.has_verified_certificate && !self.can_admit_reward_only(&cert_type) {
            return false;
        }
        let reward_only = !state.has_verified_certificate;
        if self
            .states
            .insert(cert_type, state)
            .is_some_and(|old| !old.has_verified_certificate)
        {
            self.reward_only_budget_states = self.reward_only_budget_states.saturating_sub(1);
        }
        if reward_only {
            self.reward_only_budget_states = self.reward_only_budget_states.saturating_add(1);
        }
        true
    }

    fn prune(&mut self, root_slot: Slot) {
        self.states
            .retain(|cert, _| reward_window_open(cert.slot(), root_slot));
        self.reward_only_budget_states = self
            .states
            .values()
            .filter(|state| !state.has_verified_certificate)
            .count();
    }
}

#[derive(Clone, Copy)]
enum CertificateCacheAction {
    Insert,
    Replace(usize),
    Duplicate,
    NoImprovement,
}

/// An unverified bitmap may claim useful coverage, but never evicts trusted
/// evidence until its signature has verified. Keep both union coverage and the
/// strength of individual proofs when choosing a replacement.
fn certificate_cache_action(
    cert_type: &CertificateType,
    bitmap: &[u8],
    bitmaps: &[Vec<u8>],
    root_bank: &Bank,
) -> CertificateCacheAction {
    if bitmaps.iter().any(|existing| existing == bitmap) {
        return CertificateCacheAction::Duplicate;
    }
    let Some(rank_map) = root_bank.get_rank_map(cert_type.slot()) else {
        return CertificateCacheAction::NoImprovement;
    };
    let ranks = |bitmap: &[u8]| {
        certificate_reward_vote(cert_type, bitmap, rank_map.len())?;
        let mut ranks = match decode(bitmap, rank_map.len()).ok()? {
            Decoded::Base2(ranks) | Decoded::Base3(ranks, _) => ranks,
        };
        ranks.resize(rank_map.len(), false);
        Some(ranks)
    };
    let Some(candidate) = ranks(bitmap) else {
        return CertificateCacheAction::NoImprovement;
    };
    let existing = bitmaps
        .iter()
        .map(|bitmap| ranks(bitmap).unwrap_or_else(|| BitVec::<u8>::repeat(false, rank_map.len())))
        .collect::<Vec<_>>();
    if existing.iter().any(|ranks| ranks == &candidate) {
        return CertificateCacheAction::Duplicate;
    }
    // A subset of a retained signer set pays no one new and extends no further.
    if existing
        .iter()
        .any(|ranks| candidate.iter_ones().all(|rank| ranks[rank]))
    {
        return CertificateCacheAction::NoImprovement;
    }
    if bitmaps.len() < MAX_REWARD_CERTIFICATES_PER_TYPE {
        return CertificateCacheAction::Insert;
    }
    let stake = |ranks: &BitVec<u8>| {
        ranks.iter_ones().fold(0u64, |stake, rank| {
            let entry = rank_map
                .get_pubkey_stake_entry(rank)
                .expect("ranks are sized by the rank map");
            stake.saturating_add(entry.stake.get())
        })
    };
    let mut union = BitVec::<u8>::repeat(false, rank_map.len());
    for ranks in &existing {
        union |= ranks;
    }
    let union_stake = stake(&union);
    let candidate_stake = stake(&candidate);
    let replacement = existing
        .iter()
        .enumerate()
        .filter_map(|(victim, ranks)| {
            let mut union = candidate.clone();
            for (index, existing) in existing.iter().enumerate() {
                if index != victim {
                    union |= existing;
                }
            }
            let replacement_union_stake = stake(&union);
            let victim_stake = stake(ranks);
            (replacement_union_stake > union_stake
                || (replacement_union_stake == union_stake && candidate_stake > victim_stake))
                .then_some((
                    replacement_union_stake,
                    candidate_stake.saturating_sub(victim_stake),
                    victim,
                ))
        })
        .max();
    replacement.map_or(CertificateCacheAction::NoImprovement, |(_, _, index)| {
        CertificateCacheAction::Replace(index)
    })
}

/// Signer sets the reward verifier retains, by type. The consensus loop reads them to drop copies
/// before those use up a batch's per-type limit.
#[derive(Clone, Default)]
pub(super) struct RetainedRewardBitmaps(Arc<RwLock<HashMap<CertificateType, Vec<Vec<u8>>>>>);

impl RetainedRewardBitmaps {
    /// Whether `bitmap` is exactly a signer set retained for `cert_type`. A copy cannot add
    /// coverage whatever its signature. This is only a byte comparison, so it is cheap enough for
    /// the consensus loop; the reward verifier judges everything else on its own thread. The
    /// consensus loop never waits for the lock: a copy that arrives during an update gets through,
    /// and the reward verifier drops it without verifying it.
    pub(super) fn contains(&self, cert_type: &CertificateType, bitmap: &[u8]) -> bool {
        let retained = match self.0.try_read() {
            Ok(retained) => retained,
            // A panicked writer leaves its last complete update, which is still a valid filter.
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        retained
            .get(cert_type)
            .is_some_and(|bitmaps| bitmaps.iter().any(|retained| retained == bitmap))
    }

    pub(super) fn publish(&self, cert_type: CertificateType, bitmaps: &[Vec<u8>]) {
        if !bitmaps.is_empty() {
            self.0
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(cert_type, bitmaps.to_vec());
        }
    }

    fn prune(&self, root_slot: Slot) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|cert_type, _| reward_window_open(cert_type.slot(), root_slot));
    }
}

pub(super) struct CertPayload {
    pub(super) cert: UnverifiedCertificate,
    pub(super) sender_identity_pubkey: Pubkey,
    /// Whether this certificate's signer set can serve as reward evidence for this node.
    pub(super) wants_rewards: bool,
}

/// Certificates received in one batch, grouped by type.
#[derive(Default)]
pub(super) struct CertGroups {
    /// Candidates for the first consensus proof of their type.
    pub(super) consensus: HashMap<CertificateType, Vec<CertPayload>>,
    /// Candidates of types that need no consensus proof, but may still add reward signers.
    pub(super) rewards: HashMap<CertificateType, Vec<CertPayload>>,
}

/// The first consensus proof of a type, and later candidates that may still add reward signers.
struct VerifiedConsensusCertificate {
    cert: Certificate,
    wants_rewards: bool,
    reward_candidates: Vec<CertPayload>,
}

struct CertVerifyOutcome {
    verified: Option<VerifiedConsensusCertificate>,
    failures: Vec<(BlsCertVerifyError, Pubkey)>,
    /// Later candidates useful for neither consensus nor rewards.
    redundant: usize,
    /// Later reward candidates beyond the per-type limit.
    reward_candidate_cap_dropped: usize,
}

/// Verifies certificates and sends the verified certificates to the consensus pool.
///
/// Additionally inserts valid [`CertificateType`]s into `verified_certs_set`.
/// Any certificate that fails verification will have its sender banlisted.
///
/// Function expects that the caller has already filtered any certs that appear in the
/// [`verified_certs_set`] and grouped certs with the same [`CertificateType`]. Each group is
/// verified until the first valid candidate is found. Reward evidence, including later candidates
/// of a verified type, is handed to the reward verifier after the consensus pool send. Consensus
/// never waits for optional verification, and reward-only certificates never reach the pool.
pub(super) fn verify_and_send_certificates(
    my_pubkey: &Pubkey,
    verified_certs_set: &mut HashSet<CertificateType>,
    cert_groups: CertGroups,
    root_bank: &Bank,
    reward_verifier: Option<&mut RewardCertificateVerifier>,
    channel_to_pool: &Sender<SigVerifiedBatch>,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> Result<SigVerifyCertStats, SigVerifyCertError> {
    let CertGroups { consensus, rewards } = cert_groups;
    for cert_type in consensus.keys() {
        debug_assert!(cert_type.slot() <= root_bank.slot().saturating_add(NUM_SLOTS_FOR_VERIFY));
        debug_assert!(!verified_certs_set.contains(cert_type));
    }
    let mut measure = Measure::start("verify_and_send_certificates");
    let mut stats = SigVerifyCertStats::default();

    if consensus.is_empty() && rewards.is_empty() {
        return Ok(stats);
    }

    let verified_certs = verify_cert_groups(
        consensus,
        root_bank,
        verified_certs_set,
        &mut stats,
        ban_sender,
        thread_pool,
    );
    stats.sig_verified_certs += verified_certs.len() as u64;
    let mut reward_inputs = Vec::new();
    let mut certs = Vec::with_capacity(verified_certs.len());
    for VerifiedConsensusCertificate {
        cert,
        wants_rewards,
        reward_candidates,
    } in verified_certs
    {
        if wants_rewards {
            reward_inputs.push(RewardCertificateInput::Verified(cert.clone()));
        }
        if !reward_candidates.is_empty() {
            reward_inputs.push(RewardCertificateInput::Candidates(
                cert.cert_type,
                reward_candidates,
            ));
        }
        certs.push(cert);
    }
    send_certs_to_pool(my_pubkey, certs, channel_to_pool, &mut stats.pool_sender)?;
    reward_inputs.extend(
        rewards.into_iter().map(|(cert_type, candidates)| {
            RewardCertificateInput::Candidates(cert_type, candidates)
        }),
    );
    // Without reward recovery, admission marks nothing as wanted for rewards.
    if let Some(reward_verifier) = reward_verifier {
        for input in reward_inputs {
            reward_verifier.send(input, &mut stats);
        }
    }

    measure.stop();
    stats
        .fn_verify_and_send_certs_stats
        .add_sample(measure.as_us());
    Ok(stats)
}

/// Verifies certificates in `cert_groups`, stores a local copy, and prepares them for forwarding.
///
/// The valid certs are inserted into the [`verified_certs_set`].
/// Invalid cert senders are banlisted.
/// Returns the first valid certificate of each type, with any later reward candidates.
fn verify_cert_groups(
    cert_groups: HashMap<CertificateType, Vec<CertPayload>>,
    root_bank: &Bank,
    verified_certs_set: &mut HashSet<CertificateType>,
    stats: &mut SigVerifyCertStats,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> Vec<VerifiedConsensusCertificate> {
    let results = thread_pool.install(|| {
        cert_groups
            .into_par_iter()
            .map(|(_, certs)| verify_cert_group(certs, root_bank, thread_pool))
            .collect::<Vec<_>>()
    });

    let mut certs = Vec::new();
    for outcome in results {
        let num_certs_attempted = if outcome.verified.is_some() {
            outcome.failures.len().saturating_add(1)
        } else {
            outcome.failures.len()
        };
        stats.certs_to_sig_verify += num_certs_attempted as u64;
        stats.redundant_certs_skipped += outcome.redundant as u64;
        stats.reward_candidate_cap_dropped += outcome.reward_candidate_cap_dropped as u64;

        let banned = ban_senders(outcome.failures, ban_sender);
        stats.banning_validator += banned;
        stats.certificate_verification_failed += banned;

        if let Some(verified) = outcome.verified {
            if verified_certs_set.insert(verified.cert.cert_type) {
                certs.push(verified);
            } else {
                stats.unnecessary_certs_verified += 1;
            }
        }
    }

    certs
}

fn verify_cert_group(
    certs: Vec<CertPayload>,
    root_bank: &Bank,
    thread_pool: &ThreadPool,
) -> CertVerifyOutcome {
    let mut failures = Vec::new();
    let mut certs = certs.into_iter();

    while let Some(cert_payload) = certs.next() {
        match verify_cert(cert_payload.cert, root_bank, Some(thread_pool)) {
            Ok(cert) => {
                // Later candidates cannot help consensus, but may add signers for rewards. A
                // relayed copy of the verified signer set cannot, and neither can a repeat of a
                // kept candidate, which must not use up the per-type limit.
                let mut reward_candidates: Vec<CertPayload> = Vec::new();
                let mut redundant = 0usize;
                let mut reward_candidate_cap_dropped = 0usize;
                for payload in certs.by_ref() {
                    if !payload.wants_rewards
                        || payload.cert.bitmap == cert.bitmap
                        || reward_candidates.iter().any(|kept| {
                            kept.cert.bitmap == payload.cert.bitmap
                                && kept.cert.signature == payload.cert.signature
                        })
                    {
                        redundant = redundant.saturating_add(1);
                    } else if reward_candidates.len() < MAX_CERTIFICATE_CANDIDATES_PER_TYPE {
                        reward_candidates.push(payload);
                    } else {
                        reward_candidate_cap_dropped =
                            reward_candidate_cap_dropped.saturating_add(1);
                    }
                }
                return CertVerifyOutcome {
                    verified: Some(VerifiedConsensusCertificate {
                        cert,
                        wants_rewards: cert_payload.wants_rewards,
                        reward_candidates,
                    }),
                    failures,
                    redundant,
                    reward_candidate_cap_dropped,
                };
            }
            Err(err) => failures.push((err, cert_payload.sender_identity_pubkey)),
        }
    }

    CertVerifyOutcome {
        verified: None,
        failures,
        redundant: 0,
        reward_candidate_cap_dropped: 0,
    }
}

fn verify_cert(
    cert: UnverifiedCertificate,
    root_bank: &Bank,
    thread_pool: Option<&ThreadPool>,
) -> Result<Certificate, BlsCertVerifyError> {
    let cert_slot = cert.cert_type.slot();
    let root_slot = root_bank.slot();
    debug_assert!(cert_slot <= root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY));
    root_bank.verify_certificate(cert, thread_pool)
}

/// Bans the senders of invalid certificates and returns how many were banned.
fn ban_senders(failures: Vec<(BlsCertVerifyError, Pubkey)>, ban_sender: &BanSender) -> u64 {
    let banned = failures.len() as u64;
    for (err, sender_identity_pubkey) in failures {
        ban_sender.ban(sender_identity_pubkey, BAN_TIMEOUT);
        info!("bls_cert_sigverify: banned sender={sender_identity_pubkey} due to error {err}");
    }
    banned
}

/// Work for the reward verifier.
pub(super) enum RewardCertificateInput {
    /// A certificate verified on the consensus path, which is not verified again.
    Verified(Certificate),
    /// Unverified certificates of one type, which are useful only as reward evidence.
    Candidates(CertificateType, Vec<CertPayload>),
}

/// Verifies optional reward evidence on its own thread, so the consensus loop only enqueues it.
pub(super) struct RewardCertificateVerifier {
    sender: Option<Sender<RewardCertificateInput>>,
    worker: Option<JoinHandle<()>>,
    retained: RetainedRewardBitmaps,
}

impl RewardCertificateVerifier {
    pub(super) fn new(
        sharable_banks: SharableBanks,
        cluster_info: Arc<ClusterInfo>,
        leader_schedule: Arc<LeaderScheduleCache>,
        channel_to_reward: Sender<RewardInput>,
        ban_sender: BanSender,
    ) -> Self {
        let (sender, receiver) = bounded(MAX_PENDING_REWARD_INPUTS);
        let retained = RetainedRewardBitmaps::default();
        let worker = RewardCertificateWorker {
            receiver,
            sharable_banks,
            cluster_info,
            leader_schedule,
            channel_to_reward,
            ban_sender,
            certificates: RewardCertificates::new(retained.clone()),
            reward_certificate_sender: RewardCertificateSender::new(),
        };
        let worker = thread::Builder::new()
            .name("solSigVerBLSRwd".to_string())
            .spawn(move || worker.run())
            .unwrap();
        Self {
            sender: Some(sender),
            worker: Some(worker),
            retained,
        }
    }

    pub(super) fn retained(&self) -> &RetainedRewardBitmaps {
        &self.retained
    }

    /// Never blocks. While the verifier is behind, new reward evidence is dropped.
    fn send(&mut self, input: RewardCertificateInput, stats: &mut SigVerifyCertStats) {
        let Some(sender) = &self.sender else {
            stats.reward_inputs_dropped += 1;
            return;
        };
        if matches!(input, RewardCertificateInput::Candidates(..))
            && sender.len() >= MAX_PENDING_REWARD_CANDIDATES
        {
            stats.reward_inputs_dropped += 1;
            return;
        }
        match sender.try_send(input) {
            Ok(()) => stats.reward_inputs_sent += 1,
            Err(TrySendError::Full(_)) => stats.reward_inputs_dropped += 1,
            Err(TrySendError::Disconnected(_)) => {
                // The verifier only exits early by panicking, which a validator's panic hook
                // turns into a process exit. Consensus does not depend on it.
                error!("bls_cert_sigverify: reward certificate verifier exited");
                stats.reward_inputs_dropped += 1;
                self.sender = None;
            }
        }
    }
}

impl Drop for RewardCertificateVerifier {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct RewardCertificateWorker {
    receiver: Receiver<RewardCertificateInput>,
    sharable_banks: SharableBanks,
    cluster_info: Arc<ClusterInfo>,
    leader_schedule: Arc<LeaderScheduleCache>,
    channel_to_reward: Sender<RewardInput>,
    ban_sender: BanSender,
    certificates: RewardCertificates,
    reward_certificate_sender: RewardCertificateSender,
}

impl RewardCertificateWorker {
    fn run(mut self) {
        loop {
            let root_slot = self.sharable_banks.root().slot();
            self.certificates.prune(root_slot);
            self.reward_certificate_sender
                .retry(root_slot, &self.channel_to_reward);
            self.reward_certificate_sender.maybe_report();
            self.certificates.stats.maybe_report();
            let input = match self
                .receiver
                .recv_timeout(self.reward_certificate_sender.poll_interval())
            {
                Ok(input) => input,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let root_bank = self.sharable_banks.root();
            let verified_certs = match input {
                RewardCertificateInput::Verified(cert) => {
                    self.certificates.remember(&cert, &root_bank);
                    vec![cert]
                }
                RewardCertificateInput::Candidates(cert_type, candidates) => self
                    .certificates
                    .verify(cert_type, candidates, &root_bank, &self.ban_sender),
            };
            self.reward_certificate_sender.send(
                &self.cluster_info.id(),
                &self.leader_schedule,
                &root_bank,
                &verified_certs,
                &self.channel_to_reward,
            );
        }
        self.reward_certificate_sender.report();
        self.certificates.stats.report();
    }
}

/// Reward signer sets and budgets, owned by the reward verifier thread.
struct RewardCertificates {
    verified_certs: VerifiedCertificates,
    retained: RetainedRewardBitmaps,
    last_pruned_root: Slot,
    stats: RewardCertVerifierStats,
}

impl RewardCertificates {
    fn new(retained: RetainedRewardBitmaps) -> Self {
        Self {
            verified_certs: VerifiedCertificates::new(),
            retained,
            last_pruned_root: 0,
            stats: RewardCertVerifierStats::default(),
        }
    }

    fn prune(&mut self, root_slot: Slot) {
        if self.last_pruned_root < root_slot {
            self.last_pruned_root = root_slot;
            self.verified_certs.prune(root_slot);
            self.retained.prune(root_slot);
        }
    }

    /// Records a certificate verified on the consensus path, so later candidates are compared
    /// against its signer set.
    fn remember(&mut self, cert: &Certificate, root_bank: &Bank) {
        self.stats.verified_inputs += 1;
        if !reward_window_open(cert.cert_type.slot(), root_bank.slot()) {
            return;
        }
        let mut state = self
            .verified_certs
            .get(&cert.cert_type)
            .cloned()
            .unwrap_or_else(VerifiedCertificateState::new);
        state.has_verified_certificate = true;
        let changed = match certificate_cache_action(
            &cert.cert_type,
            &cert.bitmap,
            &state.bitmaps,
            root_bank,
        ) {
            CertificateCacheAction::Insert => {
                state.bitmaps.push(cert.bitmap.clone());
                true
            }
            CertificateCacheAction::Replace(index) => {
                state.bitmaps[index] = cert.bitmap.clone();
                true
            }
            CertificateCacheAction::Duplicate | CertificateCacheAction::NoImprovement => false,
        };
        // The consensus loop reads the retained sets, so only update them when they change.
        if changed {
            self.retained.publish(cert.cert_type, &state.bitmaps);
        }
        self.verified_certs.insert(cert.cert_type, state);
    }

    /// Verifies reward candidates of one type within the per-type and shared budgets, bans the
    /// senders of invalid certificates, and returns the certificates that verified.
    fn verify(
        &mut self,
        cert_type: CertificateType,
        candidates: Vec<CertPayload>,
        root_bank: &Bank,
        ban_sender: &BanSender,
    ) -> Vec<Certificate> {
        self.stats.candidates_received += candidates.len() as u64;
        // A candidate that waited past its reward window cannot be used, so it costs no budget.
        if !reward_window_open(cert_type.slot(), root_bank.slot()) {
            self.stats.late_candidates += candidates.len() as u64;
            return Vec::new();
        }
        let known = self.verified_certs.contains_key(&cert_type);
        if !self.verified_certs.can_admit_reward_only(&cert_type) {
            self.stats.budget_cache_full += 1;
            return Vec::new();
        }
        let now = Instant::now();
        self.verified_certs.refill_reward_budget(now);
        let mut state = self
            .verified_certs
            .get(&cert_type)
            .cloned()
            .unwrap_or_else(VerifiedCertificateState::new);
        state.refill(now);
        let outcome = verify_reward_group(
            candidates,
            root_bank,
            state,
            &mut self.verified_certs.remaining_reward_attempts,
        );
        self.stats.certs_to_sig_verify += outcome.attempted as u64;
        self.stats.sig_verified_certs += outcome.verified_certs.len() as u64;
        self.stats.candidate_cap_dropped += outcome.candidate_cap_dropped as u64;
        self.stats.coverage_dropped += outcome.coverage_dropped as u64;
        self.stats.budget_dropped += outcome.budget_dropped as u64;
        self.stats.global_budget_dropped += outcome.global_budget_dropped as u64;
        self.stats.certificate_verification_failed += ban_senders(outcome.failures, ban_sender);
        // A failed-only type keeps its spent budget, so a later batch cannot start it afresh.
        if known
            || outcome.state.has_verified_certificate
            || outcome.state.remaining_reward_attempts < MAX_CERTIFICATE_CANDIDATES_PER_TYPE
        {
            // Only a verified candidate changes the retained sets.
            let bitmaps =
                (!outcome.verified_certs.is_empty()).then(|| outcome.state.bitmaps.clone());
            if self.verified_certs.insert(cert_type, outcome.state) {
                if let Some(bitmaps) = bitmaps {
                    self.retained.publish(cert_type, &bitmaps);
                }
            } else {
                self.stats.budget_cache_full += 1;
            }
        }
        outcome.verified_certs
    }
}

struct RewardVerifyOutcome {
    verified_certs: Vec<Certificate>,
    failures: Vec<(BlsCertVerifyError, Pubkey)>,
    attempted: usize,
    state: VerifiedCertificateState,
    candidate_cap_dropped: usize,
    coverage_dropped: usize,
    budget_dropped: usize,
    global_budget_dropped: usize,
}

/// Verifies only candidates that add useful reward coverage, within the per-type budget in
/// `state` and the shared `remaining_reward_attempts`. Up to four useful signer sets are retained,
/// with verified replacements allowed when coverage improves.
fn verify_reward_group(
    certs: Vec<CertPayload>,
    root_bank: &Bank,
    mut state: VerifiedCertificateState,
    remaining_reward_attempts: &mut usize,
) -> RewardVerifyOutcome {
    let mut verified_certs = Vec::new();
    let mut failures = Vec::new();
    let mut attempted = 0usize;
    let mut candidate_cap_dropped = 0usize;
    let mut coverage_dropped = 0usize;
    let mut budget_dropped = 0usize;
    let mut global_budget_dropped = 0usize;
    let num_candidates = certs.len();

    for (index, cert_payload) in certs.into_iter().enumerate() {
        if index >= MAX_CERTIFICATE_CANDIDATES_PER_TYPE {
            candidate_cap_dropped = num_candidates.saturating_sub(index);
            break;
        }
        if state.remaining_reward_attempts == 0 {
            budget_dropped = num_candidates.saturating_sub(index);
            break;
        }
        if *remaining_reward_attempts == 0 {
            global_budget_dropped = num_candidates.saturating_sub(index);
            break;
        }
        let cache_action = certificate_cache_action(
            &cert_payload.cert.cert_type,
            &cert_payload.cert.bitmap,
            &state.bitmaps,
            root_bank,
        );
        match cache_action {
            CertificateCacheAction::Duplicate => continue,
            CertificateCacheAction::NoImprovement => {
                coverage_dropped = coverage_dropped.saturating_add(1);
                continue;
            }
            CertificateCacheAction::Insert | CertificateCacheAction::Replace(_) => (),
        }
        *remaining_reward_attempts = remaining_reward_attempts.saturating_sub(1);
        state.remaining_reward_attempts = state.remaining_reward_attempts.saturating_sub(1);
        attempted = attempted.saturating_add(1);
        match verify_cert(cert_payload.cert, root_bank, None) {
            Ok(cert) => {
                state.has_verified_certificate = true;
                match cache_action {
                    CertificateCacheAction::Insert => state.bitmaps.push(cert.bitmap.clone()),
                    CertificateCacheAction::Replace(index) => {
                        state.bitmaps[index] = cert.bitmap.clone();
                    }
                    CertificateCacheAction::Duplicate | CertificateCacheAction::NoImprovement => {
                        unreachable!("only useful candidates are verified")
                    }
                }
                verified_certs.push(cert);
            }
            Err(err) => failures.push((err, cert_payload.sender_identity_pubkey)),
        }
    }

    RewardVerifyOutcome {
        verified_certs,
        failures,
        attempted,
        state,
        candidate_cap_dropped,
        coverage_dropped,
        budget_dropped,
        global_budget_dropped,
    }
}

#[cfg(test)]
impl RewardCertificateVerifier {
    fn from_sender(sender: Sender<RewardCertificateInput>) -> Self {
        Self {
            sender: Some(sender),
            worker: None,
            retained: RetainedRewardBitmaps::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::sig_verified_messages::SigVerifiedBatch,
        agave_bls_cert_verify::cert_verify::test_create_base2_unverified_certificate,
        agave_votor_transport::endpoint::stub_ban_channel_for_tests,
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Keypair as BLSKeypair, Signature},
        solana_runtime::genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts,
        },
    };

    fn fixture() -> (Bank, Vec<BLSKeypair>, ThreadPool) {
        let validators = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_vote_accounts(
            1_000_000_000,
            &validators,
            (0..validators.len())
                .map(|rank| 1_000u64.saturating_sub(rank as u64))
                .collect(),
        );
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        let keypairs = validators
            .iter()
            .map(|validator| validator.bls_keypair.clone())
            .collect::<Vec<_>>();
        let thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        (bank, keypairs, thread_pool)
    }

    fn payload(cert: UnverifiedCertificate) -> CertPayload {
        CertPayload {
            cert,
            sender_identity_pubkey: Pubkey::new_unique(),
            wants_rewards: true,
        }
    }

    fn invalid(
        cert: &UnverifiedCertificate,
        keypair: &BLSKeypair,
        message: &[u8],
    ) -> UnverifiedCertificate {
        let mut invalid = cert.clone();
        invalid.signature = keypair.sign(message).into();
        invalid
    }

    fn verify_reward_candidates(
        certs: Vec<CertPayload>,
        bank: &Bank,
        state: VerifiedCertificateState,
    ) -> RewardVerifyOutcome {
        let mut remaining_reward_attempts = MAX_REWARD_VERIFICATIONS_PER_INTERVAL;
        verify_reward_group(certs, bank, state, &mut remaining_reward_attempts)
    }

    #[test]
    fn test_reward_alternative_verification_budget() {
        let (bank, keypairs, thread_pool) = fixture();
        let cert_type = CertificateType::new_unique_notar(10);
        let first = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let alternative = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(1..8).collect::<Vec<_>>(),
        );
        let candidates = || {
            let mut candidates = (0..MAX_CERTIFICATE_CANDIDATES_PER_TYPE)
                .map(|i| payload(invalid(&alternative, &keypairs[0], &i.to_le_bytes())))
                .collect::<Vec<_>>();
            candidates.push(payload(alternative.clone()));
            candidates
        };
        // Cover reward-only searches and an already known external certificate.
        for existing in [Vec::new(), vec![first.bitmap.clone()]] {
            let mut state = VerifiedCertificateState::new();
            state.has_verified_certificate = !existing.is_empty();
            state.bitmaps = existing;
            let outcome = verify_reward_candidates(candidates(), &bank, state);
            assert!(outcome.verified_certs.is_empty());
            assert_eq!(outcome.attempted, MAX_CERTIFICATE_CANDIDATES_PER_TYPE);
            assert_eq!(outcome.failures.len(), MAX_CERTIFICATE_CANDIDATES_PER_TYPE);
            assert_eq!(outcome.candidate_cap_dropped, 1);
        }
        // The first consensus proof is found without reward limits. Only the per-type number
        // of later candidates is handed on, and those keep the reward limits.
        let mut certs = vec![payload(first.clone())];
        certs.extend(candidates());
        let outcome = verify_cert_group(certs, &bank, &thread_pool);
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.reward_candidate_cap_dropped, 1);
        let verified = outcome.verified.unwrap();
        assert_eq!(verified.cert.bitmap, first.bitmap);
        assert_eq!(
            verified.reward_candidates.len(),
            MAX_CERTIFICATE_CANDIDATES_PER_TYPE,
        );
        let mut state = VerifiedCertificateState::new();
        state.has_verified_certificate = true;
        state.bitmaps = vec![first.bitmap.clone()];
        let rewards = verify_reward_candidates(verified.reward_candidates, &bank, state);
        assert!(rewards.verified_certs.is_empty());
        assert_eq!(rewards.attempted, MAX_CERTIFICATE_CANDIDATES_PER_TYPE);
    }

    #[test]
    fn test_reward_certificate_budget_refills_for_broader_signer_set() {
        let (bank, keypairs, _thread_pool) = fixture();
        let cert_type = CertificateType::new_unique_notar(10);
        let create = |ranks: Vec<_>| {
            test_create_base2_unverified_certificate(&keypairs, 0, cert_type, &ranks)
        };
        let bitmaps = (0usize..4)
            .map(|start| create((start..start.saturating_add(7)).collect()).bitmap)
            .collect::<Vec<_>>();
        let broader = create((0..8).collect());
        let started = Instant::now();
        let state = VerifiedCertificateState {
            bitmaps: bitmaps.clone(),
            remaining_reward_attempts: 0,
            has_verified_certificate: true,
            budget_started: started,
        };
        let mut outcome = verify_reward_candidates(vec![payload(broader.clone())], &bank, state);
        assert_eq!(outcome.attempted, 0);
        assert_eq!(outcome.budget_dropped, 1);
        assert_eq!(outcome.state.bitmaps, bitmaps);
        outcome.state.refill(
            started
                .checked_add(
                    REWARD_VERIFICATION_BUDGET_INTERVAL.saturating_sub(Duration::from_nanos(1)),
                )
                .unwrap(),
        );
        assert_eq!(outcome.state.remaining_reward_attempts, 0);
        outcome.state.refill(
            started
                .checked_add(REWARD_VERIFICATION_BUDGET_INTERVAL)
                .unwrap(),
        );
        assert_eq!(
            outcome.state.remaining_reward_attempts,
            MAX_CERTIFICATE_CANDIDATES_PER_TYPE,
        );

        let failed = verify_reward_candidates(
            vec![payload(invalid(&broader, &keypairs[0], b"wrong payload"))],
            &bank,
            outcome.state.clone(),
        );
        assert_eq!(failed.failures.len(), 1);
        assert_eq!(failed.state.bitmaps, bitmaps);

        let accepted =
            verify_reward_candidates(vec![payload(broader.clone())], &bank, outcome.state);
        assert_eq!(accepted.verified_certs.len(), 1);
        assert_eq!(
            accepted.state.bitmaps.len(),
            MAX_REWARD_CERTIFICATES_PER_TYPE
        );
        assert!(accepted.state.bitmaps.contains(&broader.bitmap));
    }

    #[test]
    fn test_subsets_of_retained_signer_sets_are_not_verified() {
        let (bank, keypairs, _thread_pool) = fixture();
        let cert_type = CertificateType::new_unique_notar(10);
        let create = |ranks: Vec<_>| {
            test_create_base2_unverified_certificate(&keypairs, 0, cert_type, &ranks)
        };
        // Every signer set meets the certificate threshold.
        let retained = create((0..8).collect());
        let subset = create((0..7).collect());
        let overlapping = create((3..10).collect());
        let mut state = VerifiedCertificateState::new();
        state.has_verified_certificate = true;
        state.bitmaps = vec![retained.bitmap.clone()];
        let outcome = verify_reward_candidates(
            vec![payload(subset), payload(overlapping.clone())],
            &bank,
            state,
        );
        // Only the signer set that pays someone new is verified and retained.
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.attempted, 1);
        assert_eq!(outcome.coverage_dropped, 1);
        assert_eq!(
            outcome.state.bitmaps,
            vec![retained.bitmap, overlapping.bitmap]
        );
    }

    #[test]
    fn test_failed_reward_only_budget_is_retained_between_receives() {
        let (bank, keypairs, _thread_pool) = fixture();
        let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1_024);
        let cert_type = CertificateType::new_unique_notar(10);
        let valid = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let invalid_candidates = (0..MAX_CERTIFICATE_CANDIDATES_PER_TYPE)
            .map(|index| payload(invalid(&valid, &keypairs[0], &index.to_le_bytes())))
            .collect();
        let mut certificates = RewardCertificates::new(RetainedRewardBitmaps::default());
        assert!(
            certificates
                .verify(cert_type, invalid_candidates, &bank, &ban_sender)
                .is_empty()
        );
        let state = certificates.verified_certs.get(&cert_type).unwrap();
        assert!(!state.has_verified_certificate);
        assert_eq!(state.remaining_reward_attempts, 0);
        // Freeze the exhausted budget across the second batch independently
        // of how long this test is descheduled by other test threads.
        certificates
            .verified_certs
            .states
            .get_mut(&cert_type)
            .unwrap()
            .budget_started = Instant::now().checked_add(Duration::from_secs(60)).unwrap();
        assert!(
            certificates
                .verify(cert_type, vec![payload(valid)], &bank, &ban_sender)
                .is_empty()
        );
        assert_eq!(
            certificates.stats.certs_to_sig_verify.0,
            MAX_CERTIFICATE_CANDIDATES_PER_TYPE as u64
        );
        assert_eq!(certificates.stats.budget_dropped.0, 1);
    }

    #[test]
    fn test_first_consensus_proof_is_not_limited_by_reward_budgets() {
        let (bank, keypairs, thread_pool) = fixture();
        let cert_type = CertificateType::new_unique_notar(10);
        let valid = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let mut candidates = (0u64..12)
            .map(|index| payload(invalid(&valid, &keypairs[0], &index.to_le_bytes())))
            .collect::<Vec<_>>();
        candidates.push(payload(valid));
        let outcome = verify_cert_group(candidates, &bank, &thread_pool);
        assert_eq!(outcome.failures.len(), 12);
        assert!(outcome.verified.is_some());
    }

    #[test]
    fn test_consensus_proof_defers_optional_verification() {
        let (bank, keypairs, thread_pool) = fixture();
        let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1_024);
        let cert_type = CertificateType::new_unique_notar(10);
        let valid = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let alternative = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(1..8).collect::<Vec<_>>(),
        );
        // Two different forgeries, since an exact repeat would be dropped as redundant.
        let forged = |keypair| invalid(&alternative, keypair, b"wrong payload");
        let outcome = verify_cert_group(
            vec![
                payload(valid),
                payload(forged(&keypairs[0])),
                payload(forged(&keypairs[1])),
            ],
            &bank,
            &thread_pool,
        );
        assert!(outcome.failures.is_empty());
        let verified = outcome.verified.unwrap();
        assert_eq!(verified.reward_candidates.len(), 2);

        let mut certificates = RewardCertificates::new(RetainedRewardBitmaps::default());
        certificates.remember(&verified.cert, &bank);
        certificates.verified_certs.remaining_reward_attempts = 1;
        certificates.verified_certs.reward_budget_started =
            Instant::now().checked_add(Duration::from_secs(60)).unwrap();
        assert!(
            certificates
                .verify(cert_type, verified.reward_candidates, &bank, &ban_sender)
                .is_empty()
        );
        assert_eq!(certificates.stats.certs_to_sig_verify.0, 1);
        assert_eq!(certificates.stats.certificate_verification_failed.0, 1);
        assert_eq!(certificates.stats.global_budget_dropped.0, 1);
        assert_eq!(certificates.verified_certs.remaining_reward_attempts, 0);
    }

    #[test]
    fn test_relayed_copy_of_verified_signer_set_is_redundant() {
        let (bank, keypairs, thread_pool) = fixture();
        let cert_type = CertificateType::new_unique_notar(10);
        let valid = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let alternative = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(1..8).collect::<Vec<_>>(),
        );
        let outcome = verify_cert_group(
            vec![
                payload(valid.clone()),
                payload(valid),
                payload(alternative.clone()),
            ],
            &bank,
            &thread_pool,
        );
        assert_eq!(outcome.redundant, 1);
        let verified = outcome.verified.unwrap();
        assert_eq!(verified.reward_candidates.len(), 1);
        assert_eq!(
            verified.reward_candidates[0].cert.bitmap,
            alternative.bitmap
        );
    }

    #[test]
    fn test_repeated_reward_candidates_do_not_fill_the_limit() {
        let (bank, keypairs, thread_pool) = fixture();
        let cert_type = CertificateType::new_unique_notar(10);
        let certificate = |ranks: &[usize]| {
            test_create_base2_unverified_certificate(&keypairs, 0, cert_type, ranks)
        };
        let alternative = certificate(&(1..8).collect::<Vec<_>>());
        // A forged copy must not shadow the genuine certificate.
        let forged = invalid(&alternative, &keypairs[0], b"forged");
        let third = certificate(&(2..9).collect::<Vec<_>>());
        let mut certs = vec![
            payload(certificate(&(0..7).collect::<Vec<_>>())),
            payload(forged.clone()),
        ];
        certs
            .extend((0..MAX_CERTIFICATE_CANDIDATES_PER_TYPE).map(|_| payload(alternative.clone())));
        certs.push(payload(third.clone()));
        let outcome = verify_cert_group(certs, &bank, &thread_pool);
        assert_eq!(outcome.redundant, MAX_CERTIFICATE_CANDIDATES_PER_TYPE - 1);
        assert_eq!(outcome.reward_candidate_cap_dropped, 0);
        let kept = outcome
            .verified
            .unwrap()
            .reward_candidates
            .into_iter()
            .map(|candidate| (candidate.cert.bitmap, candidate.cert.signature))
            .collect::<Vec<_>>();
        assert_eq!(
            kept,
            [forged, alternative, third].map(|cert| (cert.bitmap, cert.signature)),
        );
    }

    #[test]
    fn test_reward_candidates_cannot_crowd_out_consensus_proofs() {
        let (sender, receiver) = bounded(MAX_PENDING_REWARD_INPUTS);
        let mut reward_verifier = RewardCertificateVerifier::from_sender(sender);
        let mut stats = SigVerifyCertStats::default();
        for _ in 0..MAX_PENDING_REWARD_INPUTS {
            reward_verifier.send(
                RewardCertificateInput::Candidates(CertificateType::new_unique_notar(10), vec![]),
                &mut stats,
            );
        }
        assert_eq!(receiver.len(), MAX_PENDING_REWARD_CANDIDATES);
        assert_eq!(
            stats.reward_inputs_dropped.0,
            MAX_PENDING_REWARD_INPUTS.saturating_sub(MAX_PENDING_REWARD_CANDIDATES) as u64,
        );
        let cert = Certificate {
            cert_type: CertificateType::new_unique_notar(10),
            signature: Signature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        };
        for _ in MAX_PENDING_REWARD_CANDIDATES..MAX_PENDING_REWARD_INPUTS {
            reward_verifier.send(RewardCertificateInput::Verified(cert.clone()), &mut stats);
        }
        assert_eq!(receiver.len(), MAX_PENDING_REWARD_INPUTS);
        assert_eq!(stats.reward_inputs_sent.0, MAX_PENDING_REWARD_INPUTS as u64);
    }

    #[test]
    fn test_exited_reward_verifier_does_not_affect_consensus_hand_off() {
        let (bank, keypairs, thread_pool) = fixture();
        let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1_024);
        let cert_type = CertificateType::new_unique_notar(10);
        let valid = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let alternative = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(1..8).collect::<Vec<_>>(),
        );
        let (sender, receiver) = bounded(MAX_PENDING_REWARD_INPUTS);
        drop(receiver);
        let mut reward_verifier = RewardCertificateVerifier::from_sender(sender);
        let (channel_to_pool, pool_receiver) = bounded(1);
        let stats = verify_and_send_certificates(
            &Pubkey::new_unique(),
            &mut HashSet::new(),
            CertGroups {
                consensus: HashMap::from([(
                    cert_type,
                    vec![payload(valid.clone()), payload(alternative)],
                )]),
                rewards: HashMap::new(),
            },
            &bank,
            Some(&mut reward_verifier),
            &channel_to_pool,
            &ban_sender,
            &thread_pool,
        )
        .unwrap();
        match pool_receiver.try_recv().unwrap() {
            SigVerifiedBatch::Certificates(certs) => {
                assert_eq!(certs.len(), 1);
                assert_eq!(certs[0].bitmap, valid.bitmap);
            }
            batch => panic!("unexpected batch {batch:?}"),
        }
        assert!(reward_verifier.sender.is_none());
        assert_eq!(stats.reward_inputs_sent.0, 0);
        assert_eq!(stats.reward_inputs_dropped.0, 2);
    }

    #[test]
    fn test_retained_signer_sets_filter_copies_until_the_window_closes() {
        let (bank, keypairs, _thread_pool) = fixture();
        let retained = RetainedRewardBitmaps::default();
        let mut certificates = RewardCertificates::new(retained.clone());
        let cert_type = CertificateType::new_unique_notar(10);
        let first = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let alternative = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            cert_type,
            &(1..8).collect::<Vec<_>>(),
        );
        assert!(!retained.contains(&cert_type, &first.bitmap));
        certificates.remember(&verify_cert(first.clone(), &bank, None).unwrap(), &bank);
        assert!(retained.contains(&cert_type, &first.bitmap));
        assert!(!retained.contains(&cert_type, &alternative.bitmap));
        let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(16);
        let verified = certificates.verify(
            cert_type,
            vec![payload(alternative.clone())],
            &bank,
            &ban_sender,
        );
        assert_eq!(verified.len(), 1);
        assert!(retained.contains(&cert_type, &alternative.bitmap));

        // The consensus loop does not wait while the reward verifier updates the sets.
        let (locked_sender, locked) = bounded(0);
        let (release, released) = bounded::<()>(1);
        let writer = thread::spawn({
            let retained = retained.clone();
            move || {
                let _update = retained.0.write().unwrap();
                locked_sender.send(()).unwrap();
                // Bounded, so a lookup that waits for the lock fails the test instead of hanging.
                let _ = released.recv_timeout(Duration::from_secs(5));
            }
        });
        locked.recv().unwrap();
        assert!(!retained.contains(&cert_type, &first.bitmap));
        release.send(()).unwrap();
        writer.join().unwrap();
        assert!(retained.contains(&cert_type, &first.bitmap));

        certificates.prune(18);
        assert!(!retained.contains(&cert_type, &first.bitmap));
        assert!(!retained.contains(&cert_type, &alternative.bitmap));
    }

    #[test]
    fn test_candidates_past_their_reward_window_spend_no_budget() {
        let (bank, keypairs, _thread_pool) = fixture();
        let bank_forks = solana_runtime::bank_forks::BankForks::new_rw_arc(bank);
        let root_bank = bank_forks.read().unwrap().root_bank();
        let bank =
            Bank::new_from_parent(root_bank, solana_runtime::bank::SlotLeader::default(), 18);
        let (ban_sender, mut ban_receiver) = stub_ban_channel_for_tests(1_024);
        let cert_type = CertificateType::new_unique_notar(10);
        let late = invalid(
            &test_create_base2_unverified_certificate(
                &keypairs,
                0,
                cert_type,
                &(0..7).collect::<Vec<_>>(),
            ),
            &keypairs[0],
            b"late",
        );
        let mut certificates = RewardCertificates::new(RetainedRewardBitmaps::default());
        assert!(
            certificates
                .verify(cert_type, vec![payload(late)], &bank, &ban_sender)
                .is_empty()
        );
        assert_eq!(certificates.stats.late_candidates.0, 1);
        assert_eq!(certificates.stats.certs_to_sig_verify.0, 0);
        assert_eq!(
            certificates.verified_certs.remaining_reward_attempts,
            MAX_REWARD_VERIFICATIONS_PER_INTERVAL
        );
        assert!(ban_receiver.try_recv().is_err());
    }

    #[test]
    fn test_reward_evidence_is_handed_off_without_verification_or_pool_send() {
        let (bank, keypairs, thread_pool) = fixture();
        let (ban_sender, mut ban_receiver) = stub_ban_channel_for_tests(1_024);
        let create = |cert_type, ranks: Vec<_>| {
            test_create_base2_unverified_certificate(&keypairs, 0, cert_type, &ranks)
        };
        let consensus_type = CertificateType::new_unique_notar(10);
        let first = create(consensus_type, (0..7).collect());
        let later = invalid(
            &create(consensus_type, (1..8).collect()),
            &keypairs[0],
            b"later",
        );
        let reward_type = CertificateType::new_unique_notar(10);
        let reward_only = invalid(
            &create(reward_type, (0..7).collect()),
            &keypairs[0],
            b"reward only",
        );
        let cert_groups = CertGroups {
            consensus: HashMap::from([(
                consensus_type,
                vec![payload(first.clone()), payload(later)],
            )]),
            rewards: HashMap::from([(reward_type, vec![payload(reward_only)])]),
        };
        let (reward_sender, reward_receiver) = bounded(MAX_PENDING_REWARD_INPUTS);
        let mut reward_verifier = RewardCertificateVerifier::from_sender(reward_sender);
        let (channel_to_pool, pool_receiver) = bounded(1);
        let mut verified_certs = HashSet::new();
        let stats = verify_and_send_certificates(
            &Pubkey::new_unique(),
            &mut verified_certs,
            cert_groups,
            &bank,
            Some(&mut reward_verifier),
            &channel_to_pool,
            &ban_sender,
            &thread_pool,
        )
        .unwrap();
        // Only the first consensus proof was verified on this thread, and only it reached the pool.
        assert_eq!(stats.certs_to_sig_verify.0, 1);
        assert_eq!(stats.reward_inputs_sent.0, 3);
        assert!(verified_certs.contains(&consensus_type));
        match pool_receiver.try_recv().unwrap() {
            SigVerifiedBatch::Certificates(certs) => {
                assert_eq!(certs.len(), 1);
                assert_eq!(certs[0].bitmap, first.bitmap);
            }
            batch => panic!("unexpected batch {batch:?}"),
        }
        assert!(pool_receiver.try_recv().is_err());
        assert!(ban_receiver.try_recv().is_err());
        let inputs = reward_receiver.try_iter().collect::<Vec<_>>();
        let [
            RewardCertificateInput::Verified(verified),
            RewardCertificateInput::Candidates(later_type, later),
            RewardCertificateInput::Candidates(reward_only_type, reward_only),
        ] = &inputs[..]
        else {
            panic!("unexpected reward inputs");
        };
        assert_eq!(verified.bitmap, first.bitmap);
        assert_eq!(*later_type, consensus_type);
        assert_eq!(later.len(), 1);
        assert_eq!(*reward_only_type, reward_type);
        assert_eq!(reward_only.len(), 1);
    }

    #[test]
    fn test_global_reward_budget_is_shared_across_types_and_receives() {
        let (bank, keypairs, _thread_pool) = fixture();
        let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1_024);
        let template = test_create_base2_unverified_certificate(
            &keypairs,
            0,
            CertificateType::new_unique_notar(10),
            &(0..7).collect::<Vec<_>>(),
        );
        let mut certificates = RewardCertificates::new(RetainedRewardBitmaps::default());
        // Freeze refill while exercising multiple batches, even if the test is
        // descheduled. Every candidate has a new type and plausible signer set.
        let started = Instant::now().checked_add(Duration::from_secs(60)).unwrap();
        certificates.verified_certs.reward_budget_started = started;
        let verify_batch = |certificates: &mut RewardCertificates| {
            for _ in 0..MAX_REWARD_ONLY_BUDGET_STATES.saturating_add(1) {
                let cert_type = CertificateType::new_unique_notar(10);
                let mut cert = template.clone();
                cert.cert_type = cert_type;
                assert!(
                    certificates
                        .verify(cert_type, vec![payload(cert)], &bank, &ban_sender)
                        .is_empty()
                );
            }
        };
        for batch in 0..2 {
            verify_batch(&mut certificates);
            assert_eq!(
                certificates.stats.certs_to_sig_verify.0,
                MAX_REWARD_VERIFICATIONS_PER_INTERVAL as u64,
                "batch {batch} must not create a fresh global allowance",
            );
        }
        assert_eq!(certificates.verified_certs.remaining_reward_attempts, 0);
        assert_eq!(
            certificates.verified_certs.states.len(),
            MAX_REWARD_VERIFICATIONS_PER_INTERVAL
        );
        assert!(certificates.stats.global_budget_dropped.0 > 0);
        // Pruning the per-type bookkeeping must not reset the global budget.
        certificates.prune(18);
        assert!(certificates.verified_certs.states.is_empty());
        certificates.verified_certs.refill_reward_budget(
            started
                .checked_add(
                    REWARD_VERIFICATION_BUDGET_INTERVAL.saturating_sub(Duration::from_nanos(1)),
                )
                .unwrap(),
        );
        assert_eq!(certificates.verified_certs.remaining_reward_attempts, 0);
        certificates.verified_certs.refill_reward_budget(
            started
                .checked_add(REWARD_VERIFICATION_BUDGET_INTERVAL)
                .unwrap(),
        );
        assert_eq!(
            certificates.verified_certs.remaining_reward_attempts,
            MAX_REWARD_VERIFICATIONS_PER_INTERVAL
        );
        verify_batch(&mut certificates);
        assert_eq!(
            certificates.stats.certs_to_sig_verify.0,
            MAX_REWARD_VERIFICATIONS_PER_INTERVAL.saturating_mul(2) as u64,
        );
    }

    #[test]
    fn test_failed_reward_only_budget_cache_is_bounded() {
        let mut cache = VerifiedCertificates::new();
        let failed_type = CertificateType::new_unique_notar(10);
        assert!(cache.insert(failed_type, VerifiedCertificateState::new()));
        for _ in 1..MAX_REWARD_ONLY_BUDGET_STATES {
            assert!(cache.insert(
                CertificateType::new_unique_notar(10),
                VerifiedCertificateState::new(),
            ));
        }
        assert_eq!(
            cache.reward_only_budget_states,
            MAX_REWARD_ONLY_BUDGET_STATES
        );
        let new_type = CertificateType::new_unique_notar(10);
        assert!(!cache.can_admit_reward_only(&new_type));
        assert!(!cache.insert(new_type, VerifiedCertificateState::new()));
        // Verified certificates remain admissible and are never evicted to
        // make room for untrusted failed-only bookkeeping.
        let mut verified = VerifiedCertificateState::new();
        verified.has_verified_certificate = true;
        assert!(cache.insert(new_type, verified));
        assert!(cache.contains_key(&new_type));
        assert!(cache.contains_key(&failed_type));
        cache.prune(18);
        assert_eq!(cache.reward_only_budget_states, 0);
        assert!(cache.states.is_empty());
    }
}
