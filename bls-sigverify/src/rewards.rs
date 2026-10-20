use {
    crate::sig_verified_messages::VoteAggregate,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::VoteMessage,
        reward_certificate::NUM_SLOTS_FOR_REWARD,
        vote::Vote,
    },
    crossbeam_channel::{Sender, TrySendError},
    log::warn,
    solana_bls_signatures::Signature,
    solana_clock::Slot,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_metrics::datapoint_info,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_signer_store::{Decoded, decode},
    std::{
        collections::VecDeque,
        num::Saturating,
        time::{Duration, Instant},
    },
};

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum RewardInput {
    External(Vec<VoteAggregate>),
    Own(VoteMessage),
    Certificates(Vec<Certificate>),
    /// Individual components of a verified vote batch. The components have not
    /// necessarily been verified independently of the batch.
    VoteSignatures {
        vote: Vote,
        signatures: Vec<(u16, Signature)>,
    },
}

#[must_use]
/// Returns whether this node can still use ordinary votes from this certificate
/// type for rewards. The bitmap must also describe a pure ordinary-vote payload.
pub(crate) fn rewards_wants_certificate(
    my_pubkey: &Pubkey,
    leader_schedule: &LeaderScheduleCache,
    root_slot: Slot,
    cert_type: &CertificateType,
) -> bool {
    matches!(
        cert_type,
        CertificateType::Notarize(_)
            | CertificateType::FinalizeFast(_)
            | CertificateType::NotarizeFallback(_)
            | CertificateType::Skip(_)
    ) && vote_is_relevant_for_rewards(my_pubkey, leader_schedule, root_slot, cert_type.slot())
}

/// Whether rewards for votes in `source_slot` can still be published with `root_slot` rooted.
/// The leader of `source_slot + NUM_SLOTS_FOR_REWARD` publishes them, so the window closes once
/// that slot is rooted.
#[must_use]
pub(crate) fn reward_window_open(source_slot: Slot, root_slot: Slot) -> bool {
    source_slot.saturating_add(NUM_SLOTS_FOR_REWARD) > root_slot
}

#[must_use]
/// Returns the ordinary reward vote signed by every signer in a certificate.
/// Mixed primary/fallback signatures cannot be separated for ordinary rewards.
pub fn certificate_reward_vote(
    cert_type: &CertificateType,
    bitmap: &[u8],
    max_validators: usize,
) -> Option<Vote> {
    let vote = match cert_type {
        CertificateType::Notarize(block)
        | CertificateType::FinalizeFast(block)
        | CertificateType::NotarizeFallback(block) => Vote::new_notarization_vote(*block),
        CertificateType::Skip(slot) => Vote::new_skip_vote(*slot),
        CertificateType::Finalize(_) | CertificateType::Genesis(_) => return None,
    };
    match decode(bitmap, max_validators).ok()? {
        Decoded::Base2(_) => Some(vote),
        Decoded::Base3(_, fallback)
            if matches!(
                cert_type,
                CertificateType::Skip(_) | CertificateType::NotarizeFallback(_)
            ) && !fallback.any() =>
        {
            Some(vote)
        }
        Decoded::Base3(_, _) => None,
    }
}

const MAX_PENDING_REWARD_CERTIFICATES: usize = 256;
const MAX_REWARD_CERTIFICATES_PER_RETRY: usize = 32;

#[derive(Default)]
struct RewardCertificateStats {
    sent: Saturating<u64>,
    channel_full: Saturating<u64>,
    channel_disconnected: Saturating<u64>,
    disconnected_dropped: Saturating<u64>,
    pending_cap_dropped: Saturating<u64>,
    expired: Saturating<u64>,
    late_received: Saturating<u64>,
    duplicates: Saturating<u64>,
}

/// Nonblocking fanout of already-verified external certificates. A full reward
/// channel retains bounded evidence for retry without repeating verification.
pub(crate) struct RewardCertificateSender {
    pending: VecDeque<Certificate>,
    stats: RewardCertificateStats,
    last_report: Instant,
}

impl RewardCertificateSender {
    const METRIC_NAME: &'static str = "bls_cert_sigverify_rewards_sender_stats";

    pub(crate) fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            stats: RewardCertificateStats::default(),
            last_report: Instant::now(),
        }
    }

    /// Retains only useful, pure, already-verified external certificates, then
    /// attempts one bounded batch. Consensus and replay never wait for rewards.
    pub(crate) fn send(
        &mut self,
        my_pubkey: &Pubkey,
        leader_schedule: &LeaderScheduleCache,
        root_bank: &Bank,
        certificates: &[Certificate],
        channel: &Sender<RewardInput>,
    ) {
        self.expire(root_bank.slot());
        for cert in certificates {
            if matches!(
                cert.cert_type,
                CertificateType::Notarize(_)
                    | CertificateType::FinalizeFast(_)
                    | CertificateType::NotarizeFallback(_)
                    | CertificateType::Skip(_)
            ) && !reward_window_open(cert.cert_type.slot(), root_bank.slot())
            {
                self.stats.late_received += 1;
                continue;
            }
            if !rewards_wants_certificate(
                my_pubkey,
                leader_schedule,
                root_bank.slot(),
                &cert.cert_type,
            ) || !root_bank
                .get_rank_map(cert.cert_type.slot())
                .is_some_and(|map| {
                    certificate_reward_vote(&cert.cert_type, &cert.bitmap, map.len()).is_some()
                })
            {
                continue;
            }
            self.enqueue(cert);
        }
        self.retry(root_bank.slot(), channel);
    }

    fn enqueue(&mut self, cert: &Certificate) {
        if self
            .pending
            .iter()
            .any(|pending| pending.cert_type == cert.cert_type && pending.bitmap == cert.bitmap)
        {
            self.stats.duplicates += 1;
            return;
        }
        // Earlier deadlines take priority over distant future certificates.
        let index = self
            .pending
            .iter()
            .position(|pending| pending.cert_type.slot() > cert.cert_type.slot())
            .unwrap_or(self.pending.len());
        self.pending.insert(index, cert.clone());
        if self.pending.len() > MAX_PENDING_REWARD_CERTIFICATES {
            self.pending.pop_back();
            self.stats.pending_cap_dropped += 1;
        }
    }

    fn expire(&mut self, root_slot: Slot) {
        while self
            .pending
            .front()
            .is_some_and(|cert| !reward_window_open(cert.cert_type.slot(), root_slot))
        {
            self.pending.pop_front();
            self.stats.expired += 1;
        }
    }

    /// Attempts at most one batch of 32 certificates. Expiry uses the same source
    /// slot reward window as ingress admission, including on otherwise idle loops.
    pub(crate) fn retry(&mut self, root_slot: Slot, channel: &Sender<RewardInput>) {
        self.expire(root_slot);
        let count = self.pending.len().min(MAX_REWARD_CERTIFICATES_PER_RETRY);
        if count == 0 {
            return;
        }
        let certificates = self.pending.drain(..count).collect();
        match channel.try_send(RewardInput::Certificates(certificates)) {
            Ok(()) => self.stats.sent += count as u64,
            Err(TrySendError::Full(RewardInput::Certificates(certificates))) => {
                self.stats.channel_full += 1;
                for cert in certificates.into_iter().rev() {
                    self.pending.push_front(cert);
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.stats.channel_disconnected += 1;
                self.stats.disconnected_dropped += count.saturating_add(self.pending.len()) as u64;
                self.pending.clear();
                warn!(
                    "{}: reward certificate channel disconnected",
                    Self::METRIC_NAME
                );
            }
            Err(TrySendError::Full(_)) => unreachable!("only certificates are sent"),
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn poll_interval(&self) -> Duration {
        if self.has_pending() {
            Duration::from_millis(10)
        } else {
            Duration::from_secs(1)
        }
    }

    pub(crate) fn maybe_report(&mut self) {
        if self.last_report.elapsed() >= Duration::from_secs(1) {
            self.report();
        }
    }

    pub(crate) fn report(&mut self) {
        let stats = std::mem::take(&mut self.stats);
        datapoint_info!(
            Self::METRIC_NAME,
            ("sent", stats.sent.0, i64),
            ("channel_full", stats.channel_full.0, i64),
            ("channel_disconnected", stats.channel_disconnected.0, i64),
            ("disconnected_dropped", stats.disconnected_dropped.0, i64),
            ("pending_cap_dropped", stats.pending_cap_dropped.0, i64),
            ("expired", stats.expired.0, i64),
            ("late_received", stats.late_received.0, i64),
            ("duplicates", stats.duplicates.0, i64),
            ("pending", self.pending.len(), i64),
        );
        self.last_report = Instant::now();
    }
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
/// Returns true if a reward vote at the `vote_slot` is needed by this node for rewards.
pub fn vote_is_relevant_for_rewards(
    my_pubkey: &Pubkey,
    leader_schedule: &LeaderScheduleCache,
    root_slot: Slot,
    vote_slot: Slot,
) -> bool {
    if !reward_window_open(vote_slot, root_slot) {
        return false;
    }
    let Some(leader) =
        leader_schedule.slot_leader_at(vote_slot.saturating_add(NUM_SLOTS_FOR_REWARD), None)
    else {
        return false;
    };
    if &leader.id != my_pubkey {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor_messages::consensus_message::Block,
        bitvec::vec::BitVec,
        crossbeam_channel::bounded,
        solana_bls_signatures::BLS_SIGNATURE_AFFINE_SIZE,
        solana_runtime::{
            bank::SlotLeader,
            bank_forks::BankForks,
            genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
        },
        solana_signer_store::{encode_base2, encode_base3},
    };

    fn certificate(slot: Slot) -> Certificate {
        Certificate {
            cert_type: CertificateType::new_unique_notar(slot),
            signature: Signature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        }
    }

    #[test]
    fn test_reward_certificate_retry_retains_verified_evidence() {
        let (channel, receiver) = bounded(1);
        channel.send(RewardInput::Certificates(vec![])).unwrap();
        let mut sender = RewardCertificateSender::new();
        let cert = certificate(10);
        sender.enqueue(&cert);
        sender.enqueue(&cert);
        sender.retry(10, &channel);
        assert_eq!(sender.pending.len(), 1);
        assert_eq!(sender.stats.channel_full.0, 1);
        assert_eq!(sender.stats.duplicates.0, 1);
        receiver.try_recv().unwrap();
        sender.retry(10, &channel);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            RewardInput::Certificates(certs) if certs == vec![cert],
        ));
        assert!(!sender.has_pending());
        assert_eq!(sender.stats.sent.0, 1);
    }

    #[test]
    fn test_reward_certificate_retry_expires_before_sending() {
        let (channel, receiver) = bounded(1);
        channel.send(RewardInput::Certificates(vec![])).unwrap();
        let mut sender = RewardCertificateSender::new();
        sender.enqueue(&certificate(10));
        sender.retry(17, &channel);
        assert!(sender.has_pending());
        receiver.try_recv().unwrap();
        sender.retry(18, &channel);
        assert!(!sender.has_pending());
        assert_eq!(sender.stats.expired.0, 1);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn test_reward_certificate_retry_bounds_pending_and_each_batch() {
        let mut sender = RewardCertificateSender::new();
        sender.enqueue(&certificate(1_000));
        for slot in 1..=MAX_PENDING_REWARD_CERTIFICATES {
            sender.enqueue(&certificate(slot as Slot));
        }
        assert_eq!(sender.pending.len(), MAX_PENDING_REWARD_CERTIFICATES);
        assert_eq!(sender.pending.front().unwrap().cert_type.slot(), 1);
        assert_eq!(
            sender.pending.back().unwrap().cert_type.slot(),
            MAX_PENDING_REWARD_CERTIFICATES as Slot,
        );
        assert_eq!(sender.stats.pending_cap_dropped.0, 1);
        let (channel, receiver) = bounded(1);
        sender.retry(0, &channel);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            RewardInput::Certificates(certs) if certs.len() == MAX_REWARD_CERTIFICATES_PER_RETRY,
        ));
        assert_eq!(
            sender.pending.len(),
            MAX_PENDING_REWARD_CERTIFICATES.saturating_sub(MAX_REWARD_CERTIFICATES_PER_RETRY),
        );
    }

    #[test]
    fn test_reward_certificate_retry_drops_disconnected_backlog() {
        let mut sender = RewardCertificateSender::new();
        for slot in 1..=MAX_PENDING_REWARD_CERTIFICATES {
            sender.enqueue(&certificate(slot as Slot));
        }
        let (channel, receiver) = bounded(1);
        drop(receiver);
        sender.retry(0, &channel);
        assert!(!sender.has_pending());
        assert_eq!(sender.stats.channel_disconnected.0, 1);
        assert_eq!(
            sender.stats.disconnected_dropped.0,
            MAX_PENDING_REWARD_CERTIFICATES as u64,
        );
        sender.retry(0, &channel);
        assert_eq!(sender.stats.channel_disconnected.0, 1);
    }

    #[test]
    fn test_reward_certificate_sender_forwards_only_my_ordinary_certificates() {
        let validators = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let genesis =
            create_genesis_config_with_vote_accounts(1_000_000_000, &validators, vec![100; 10]);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
        let root_bank = bank_forks.read().unwrap().root_bank();
        let leader_schedule = LeaderScheduleCache::new_from_bank(&root_bank);
        let slot = 10;
        let my_pubkey = leader_schedule
            .slot_leader_at(slot + NUM_SLOTS_FOR_REWARD, None)
            .unwrap()
            .id;
        let other_slot = (slot + 1..slot + 100)
            .find(|source| {
                leader_schedule
                    .slot_leader_at(source + NUM_SLOTS_FOR_REWARD, None)
                    .is_some_and(|leader| leader.id != my_pubkey)
            })
            .unwrap();
        let mut ranks = BitVec::<u8>::repeat(false, root_bank.get_rank_map(slot).unwrap().len());
        ranks.set(0, true);
        let bitmap = encode_base2(&ranks).unwrap();
        let certificate = |cert_type| Certificate {
            cert_type,
            signature: Signature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: bitmap.clone(),
        };
        let forwarded = |receiver: &crossbeam_channel::Receiver<RewardInput>| {
            receiver
                .try_iter()
                .flat_map(|input| match input {
                    RewardInput::Certificates(certs) => certs,
                    input => panic!("unexpected reward input: {input:?}"),
                })
                .map(|cert| cert.cert_type)
                .collect::<Vec<_>>()
        };
        let block = Block::new_unique(slot);
        let (channel, receiver) = bounded(16);
        let mut sender = RewardCertificateSender::new();
        // A slow-finalization footer carries a Finalize and a Notarize certificate.
        for certificates in [
            vec![
                certificate(CertificateType::Finalize(slot)),
                certificate(CertificateType::Notarize(block)),
            ],
            vec![certificate(CertificateType::FinalizeFast(block))],
            vec![certificate(CertificateType::new_unique_notar(other_slot))],
        ] {
            sender.send(
                &my_pubkey,
                &leader_schedule,
                &root_bank,
                &certificates,
                &channel,
            );
        }
        assert_eq!(
            forwarded(&receiver),
            vec![
                CertificateType::Notarize(block),
                CertificateType::FinalizeFast(block),
            ],
        );
        // The source slot's reward window closes once the root reaches slot + 8.
        for (root, cert_type) in [
            (slot + NUM_SLOTS_FOR_REWARD - 1, CertificateType::Skip(slot)),
            (
                slot + NUM_SLOTS_FOR_REWARD,
                CertificateType::new_unique_notar(slot),
            ),
        ] {
            let bank = Bank::new_from_parent(root_bank.clone(), SlotLeader::default(), root);
            sender.send(
                &my_pubkey,
                &leader_schedule,
                &bank,
                &[certificate(cert_type)],
                &channel,
            );
        }
        assert_eq!(forwarded(&receiver), vec![CertificateType::Skip(slot)]);
        assert_eq!(sender.stats.late_received.0, 1);
    }

    #[test]
    fn certificate_reward_vote_requires_a_pure_ordinary_payload() {
        let block = Block::new_unique(10);
        let mut primary = BitVec::<u8>::repeat(false, 3);
        primary.set(0, true);
        let mut fallback = BitVec::<u8>::repeat(false, 3);
        let base2 = encode_base2(&primary).unwrap();
        let pure_base3 = encode_base3(&primary, &fallback).unwrap();
        fallback.set(1, true);
        let mixed_base3 = encode_base3(&primary, &fallback).unwrap();

        for cert_type in [
            CertificateType::Notarize(block),
            CertificateType::FinalizeFast(block),
            CertificateType::NotarizeFallback(block),
        ] {
            assert_eq!(
                certificate_reward_vote(&cert_type, &base2, 3),
                Some(Vote::new_notarization_vote(block)),
            );
        }
        for cert_type in [
            CertificateType::Skip(block.slot),
            CertificateType::NotarizeFallback(block),
        ] {
            assert_eq!(
                certificate_reward_vote(&cert_type, &pure_base3, 3),
                certificate_reward_vote(&cert_type, &base2, 3),
            );
            assert_eq!(certificate_reward_vote(&cert_type, &mixed_base3, 3), None);
        }
        assert_eq!(
            certificate_reward_vote(&CertificateType::Skip(block.slot), &base2, 3),
            Some(Vote::new_skip_vote(block.slot)),
        );
        for cert_type in [
            CertificateType::Finalize(block.slot),
            CertificateType::Genesis(block),
        ] {
            assert_eq!(certificate_reward_vote(&cert_type, &base2, 3), None);
        }
        assert_eq!(
            certificate_reward_vote(&CertificateType::Notarize(block), &pure_base3, 3),
            None,
        );
    }
}
