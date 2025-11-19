//! The BLS signature verifier.

use {
    crate::{
        bls_sigverify::{
            bls_sigverify_service::BLSSigVerifyServiceError, stats::BLSSigVerifierStats,
        },
        cluster_info_vote_listener::VerifiedVoterSlotsSender,
    },
    agave_votor::consensus_metrics::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
    agave_votor_messages::{
        consensus_message::{Certificate, CertificateType, ConsensusMessage, VoteMessage},
        vote::Vote,
    },
    ahash::{AHashMap, AHashSet},
    bitvec::prelude::{BitVec, Lsb0},
    crossbeam_channel::{Sender, TrySendError},
    rayon::{
        iter::{
            IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator,
            ParallelIterator,
        },
        ThreadPool,
    },
    solana_bls_signatures::{
        pubkey::{Pubkey as BlsPubkey, PubkeyProjective, VerifiablePubkey},
        signature::SignatureProjective,
        BlsError,
    },
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks, epoch_stakes::BLSPubkeyToRankMap},
    solana_signer_store::{decode, DecodeError},
    solana_streamer::packet::PacketBatch,
    std::{
        collections::{HashMap, HashSet},
        sync::{atomic::Ordering, Arc, RwLock},
        time::Instant,
    },
    thiserror::Error,
};

// We have about 1000 validators in the network, and some are further away, so
// preallocate vec of 1024 for normal cases.
const AVERAGE_VOTES_OR_CERTS_PER_BATCH: usize = 1024;

fn get_key_to_rank_map(bank: &Bank, slot: Slot) -> Option<&Arc<BLSPubkeyToRankMap>> {
    let stakes = bank.epoch_stakes_map();
    let epoch = bank.epoch_schedule().get_epoch(slot);
    stakes
        .get(&epoch)
        .map(|stake| stake.bls_pubkey_to_rank_map())
}

// TODO(wen): projecting to PubkeyProjective should be done in key_to_rank_map.
fn aggregate_keys_from_bitmap(
    bit_vec: &BitVec<u8, Lsb0>,
    key_to_rank_map: &Arc<BLSPubkeyToRankMap>,
) -> Result<PubkeyProjective, CertVerifyError> {
    (0..bit_vec.len())
        .into_par_iter()
        .filter(|&rank| // check if the bit is set
            bit_vec.get(rank).map(|b| *b).unwrap_or(false))
        .map(|rank| {
            key_to_rank_map
                .get_pubkey(rank)
                .ok_or(CertVerifyError::PubkeyNotFoundForRank(rank))
                .and_then(|(_, bls_pubkey)| {
                    PubkeyProjective::try_from(*bls_pubkey)
                        .map_err(|_| CertVerifyError::InvalidPubkeyForRank(rank))
                })
        })
        .try_reduce_with(|mut a, b| {
            a.aggregate_with(std::iter::once(&b))
                .map_err(|_| CertVerifyError::KeyAggregationFailed)?;
            Ok(a)
        })
        .ok_or(CertVerifyError::KeyAggregationFailed)?
}

#[derive(Debug, Error, PartialEq)]
enum CertVerifyError {
    #[error("Failed to find key to rank map for slot {0}")]
    KeyToRankMapNotFound(Slot),

    #[error("Failed to decode bitmap {0:?}")]
    BitmapDecodingFailed(DecodeError),

    #[error("Failed to aggregate public keys")]
    KeyAggregationFailed,

    #[error("Failed to serialize original vote")]
    SerializationFailed,

    #[error("The signature doesn't match")]
    SignatureVerificationFailed,

    #[error("Pubkey not found for rank {0}")]
    PubkeyNotFoundForRank(usize),

    #[error("Invalid Pubkey for rank {0}")]
    InvalidPubkeyForRank(usize),

    #[error("No source vote found for cert {0:?} return_primary {1}")]
    NoSourceVoteFound(CertificateType, bool),
}

pub struct BLSSigVerifier {
    verified_votes_sender: VerifiedVoterSlotsSender,
    message_sender: Sender<ConsensusMessage>,
    sharable_banks: SharableBanks,
    stats: BLSSigVerifierStats,
    verified_certs: RwLock<HashSet<CertificateType>>,
    vote_payload_cache: RwLock<HashMap<Vote, Arc<Vec<u8>>>>,
    consensus_metrics_sender: ConsensusMetricsEventSender,
    last_checked_root_slot: Slot,
}

impl BLSSigVerifier {
    pub fn new(
        sharable_banks: SharableBanks,
        verified_votes_sender: VerifiedVoterSlotsSender,
        message_sender: Sender<ConsensusMessage>,
        consensus_metrics_sender: ConsensusMetricsEventSender,
    ) -> Self {
        Self {
            sharable_banks,
            verified_votes_sender,
            message_sender,
            stats: BLSSigVerifierStats::new(),
            verified_certs: RwLock::new(HashSet::new()),
            vote_payload_cache: RwLock::new(HashMap::new()),
            consensus_metrics_sender,
            last_checked_root_slot: 0,
        }
    }

    // We use the custom pool in tests, global pool in production.
    fn run_batch_verify_in_pool<F, R>(pool: Option<&ThreadPool>, f: F) -> R
    where
        F: FnOnce() -> R + Send,
        R: Send,
    {
        match pool {
            Some(pool) => pool.install(f),
            None => f(),
        }
    }

    fn purge_old_votes_and_certs(&mut self, root_slot: Slot) {
        if self.last_checked_root_slot < root_slot {
            self.last_checked_root_slot = root_slot;
            self.verified_certs
                .write()
                .unwrap()
                .retain(|cert| cert.slot() > root_slot);
            self.vote_payload_cache
                .write()
                .unwrap()
                .retain(|vote, _| vote.slot() > root_slot);
        }
    }

    fn preprocess_vote(
        &mut self,
        vote_message: VoteMessage,
        root_bank: &Bank,
        consensus_metrics_to_send: &mut Vec<ConsensusMetricsEvent>,
    ) -> Option<(Pubkey, BlsPubkey)> {
        // Missing epoch states
        let Some(key_to_rank_map) = get_key_to_rank_map(root_bank, vote_message.vote.slot()) else {
            self.stats
                .received_no_epoch_stakes
                .fetch_add(1, Ordering::Relaxed);
            return None;
        };

        // Invalid rank
        let Some((solana_pubkey, bls_pubkey)) =
            key_to_rank_map.get_pubkey(vote_message.rank.into())
        else {
            self.stats.received_bad_rank.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        // Capture votes received metrics before old messages are potentially discarded below.
        consensus_metrics_to_send.push(ConsensusMetricsEvent::Vote {
            id: *solana_pubkey,
            vote: vote_message.vote,
        });
        // Only need votes newer than root slot
        if vote_message.vote.slot() <= root_bank.slot() {
            self.stats.received_old.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some((*solana_pubkey, *bls_pubkey))
    }

    fn preprocess_packets(
        &mut self,
        packet_batches: &mut [PacketBatch],
        root_bank: &Bank,
        consensus_metrics_to_send: &mut Vec<ConsensusMetricsEvent>,
    ) -> (Vec<VoteToVerify>, Vec<Certificate>) {
        // TODO(sam): ideally we want to avoid heap allocation, but let's use
        //            `Vec` for now for clarity and then optimize for the final version
        let mut votes_to_verify = Vec::with_capacity(AVERAGE_VOTES_OR_CERTS_PER_BATCH);
        let mut certs_to_verify = Vec::with_capacity(AVERAGE_VOTES_OR_CERTS_PER_BATCH);
        for mut packet in packet_batches.iter_mut().flatten() {
            self.stats.received.fetch_add(1, Ordering::Relaxed);
            if packet.meta().discard() {
                self.stats
                    .received_discarded
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }

            let message: ConsensusMessage = match packet.deserialize_slice(..) {
                Ok(msg) => msg,
                Err(_) => {
                    self.stats
                        .received_malformed
                        .fetch_add(1, Ordering::Relaxed);
                    packet.meta_mut().set_discard(true);
                    continue;
                }
            };

            match message {
                ConsensusMessage::Vote(vote_message) => {
                    if let Some((pubkey, bls_pubkey)) = self.preprocess_vote(
                        vote_message.clone(),
                        root_bank,
                        consensus_metrics_to_send,
                    ) {
                        votes_to_verify.push(VoteToVerify {
                            vote_message,
                            bls_pubkey,
                            pubkey,
                        });
                    } else {
                        packet.meta_mut().set_discard(true);
                    }
                }
                ConsensusMessage::Certificate(cert) => {
                    // Only need certs newer than root slot
                    if cert.cert_type.slot() <= root_bank.slot() {
                        self.stats.received_old.fetch_add(1, Ordering::Relaxed);
                        packet.meta_mut().set_discard(true);
                        continue;
                    }

                    if self
                        .verified_certs
                        .read()
                        .unwrap()
                        .contains(&cert.cert_type)
                    {
                        self.stats.received_verified.fetch_add(1, Ordering::Relaxed);
                        packet.meta_mut().set_discard(true);
                        continue;
                    }

                    certs_to_verify.push(cert);
                }
            }
        }
        (votes_to_verify, certs_to_verify)
    }

    pub fn verify_and_send_batches(
        &mut self,
        mut batches: Vec<PacketBatch>,
        pool: Option<&ThreadPool>,
    ) -> Result<(), BLSSigVerifyServiceError> {
        let mut verify_time = Measure::start("sigverify_batch_time");
        let mut preprocess_time = Measure::start("preprocess");
        let mut consensus_metrics_to_send = Vec::with_capacity(AVERAGE_VOTES_OR_CERTS_PER_BATCH);
        let root_bank = self.sharable_banks.root();

        self.purge_old_votes_and_certs(root_bank.slot());

        preprocess_time.stop();
        self.stats.preprocess_count.fetch_add(1, Ordering::Relaxed);
        self.stats
            .preprocess_elapsed_us
            .fetch_add(preprocess_time.as_us(), Ordering::Relaxed);

        let (votes_to_verify, certs_to_verify) =
            self.preprocess_packets(&mut batches, &root_bank, &mut consensus_metrics_to_send);

        let (votes_result, certs_result) = Self::run_batch_verify_in_pool(pool, || {
            rayon::join(
                || self.verify_and_send_votes(votes_to_verify),
                || self.verify_and_send_certificates(certs_to_verify, &root_bank),
            )
        });
        verify_time.stop();
        self.stats
            .verify_elapsed_us
            .fetch_add(verify_time.as_us(), Ordering::Relaxed);

        votes_result?;
        certs_result?;

        // Send to metrics service for metrics aggregation
        if self
            .consensus_metrics_sender
            .send((Instant::now(), consensus_metrics_to_send))
            .is_err()
        {
            warn!("could not send consensus metrics, receive side of channel is closed");
        }

        self.stats.maybe_report_stats();

        Ok(())
    }

    fn verify_and_send_votes(
        &self,
        votes_to_verify: Vec<VoteToVerify>,
    ) -> Result<(), BLSSigVerifyServiceError> {
        let verified_votes = self.verify_votes(votes_to_verify);
        self.stats
            .total_valid_packets
            .fetch_add(verified_votes.len() as u64, Ordering::Relaxed);

        let mut verified_votes_by_pubkey: AHashMap<Pubkey, AHashSet<Slot>> =
            AHashMap::with_capacity(AVERAGE_VOTES_OR_CERTS_PER_BATCH);
        self.stats
            .received_votes
            .fetch_add(verified_votes.len() as u64, Ordering::Relaxed);
        for vote in verified_votes {
            if vote.vote_message.vote.is_notarization_or_finalization()
                || vote.vote_message.vote.is_notarize_fallback()
            {
                let slot = vote.vote_message.vote.slot();
                verified_votes_by_pubkey
                    .entry(vote.pubkey)
                    .or_default()
                    .insert(slot);
            }

            // Send the BLS vote messaage to certificate pool
            match self
                .message_sender
                .try_send(ConsensusMessage::Vote(vote.vote_message))
            {
                Ok(()) => {
                    self.stats.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    self.stats.sent_failed.fetch_add(1, Ordering::Relaxed);
                }
                Err(e @ TrySendError::Disconnected(_)) => {
                    return Err(e.into());
                }
            }
        }

        // Send votes
        for (pubkey, slots) in verified_votes_by_pubkey {
            match self
                .verified_votes_sender
                .try_send((pubkey, slots.into_iter().collect()))
            {
                Ok(()) => {
                    self.stats
                        .verified_votes_sent
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    trace!("Failed to send verified vote: {e}");
                    self.stats
                        .verified_votes_sent_failed
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        Ok(())
    }

    fn verify_optimistically(
        &self,
        votes_to_verify: &Vec<VoteToVerify>,
        payloads: &[Arc<Vec<u8>>],
    ) -> Result<bool, BlsError> {
        let mut grouped_pubkeys: HashMap<&Arc<Vec<u8>>, Vec<&BlsPubkey>> = HashMap::new();
        for (v, payload) in votes_to_verify.iter().zip(payloads.iter()) {
            grouped_pubkeys
                .entry(payload)
                .or_default()
                .push(&v.bls_pubkey);
        }

        let distinct_messages = grouped_pubkeys.len();
        self.stats
            .votes_batch_distinct_messages_count
            .fetch_add(distinct_messages as u64, Ordering::Relaxed);

        let (distinct_payloads, distinct_pubkeys): (Vec<_>, Vec<_>) =
            grouped_pubkeys.into_iter().unzip();
        let aggregate_pubkeys_result: Result<Vec<PubkeyProjective>, _> = distinct_pubkeys
            .into_iter()
            .map(|pks| PubkeyProjective::par_aggregate(pks.into_par_iter()))
            .collect();
        let aggregate_pubkeys = aggregate_pubkeys_result?;

        let signatures = votes_to_verify
            .par_iter()
            .map(|v| &v.vote_message.signature);
        let aggregate_signature = SignatureProjective::par_aggregate(signatures)?;
        if distinct_messages == 1 {
            let payload_slice = distinct_payloads[0].as_slice();
            let aggregate_pubkey: PubkeyProjective = aggregate_pubkeys[0];
            aggregate_pubkey.verify_signature(&aggregate_signature, payload_slice)
        } else {
            let payload_slices: Vec<&[u8]> =
                distinct_payloads.iter().map(|p| p.as_slice()).collect();

            let aggregate_pubkeys_affine: Vec<BlsPubkey> =
                aggregate_pubkeys.into_iter().map(|pk| pk.into()).collect();

            SignatureProjective::par_verify_distinct_aggregated(
                &aggregate_pubkeys_affine,
                &aggregate_signature.into(),
                &payload_slices,
            )
        }
    }

    fn verify_votes(&self, votes_to_verify: Vec<VoteToVerify>) -> Vec<VoteToVerify> {
        if votes_to_verify.is_empty() {
            return vec![];
        }

        let payloads = votes_to_verify
            .iter()
            .map(|v| self.get_vote_payload(&v.vote_message.vote))
            .collect::<Vec<_>>();

        self.stats.votes_batch_count.fetch_add(1, Ordering::Relaxed);
        let mut votes_batch_optimistic_time = Measure::start("votes_batch_optimistic");

        if let Ok(true) = self.verify_optimistically(&votes_to_verify, &payloads) {
            votes_batch_optimistic_time.stop();
            self.stats
                .votes_batch_optimistic_elapsed_us
                .fetch_add(votes_batch_optimistic_time.as_us(), Ordering::Relaxed);
            return votes_to_verify;
        }

        // Fallback: If the batch fails, verify each vote signature individually in parallel
        // to find the invalid ones.
        //
        // TODO(sam): keep a record of which validator's vote failed to incur penalty
        let mut votes_batch_parallel_verify_time = Measure::start("votes_batch_parallel_verify");
        let verified_votes = votes_to_verify
            .into_par_iter()
            .zip(payloads.par_iter())
            .filter(|(vote_to_verify, payload)| {
                if vote_to_verify
                    .bls_pubkey
                    .verify_signature(&vote_to_verify.vote_message.signature, payload.as_slice())
                    .unwrap_or(false)
                {
                    true
                } else {
                    self.stats
                        .received_bad_signature_votes
                        .fetch_add(1, Ordering::Relaxed);
                    false
                }
            })
            .map(|(v, _)| v)
            .collect();
        votes_batch_parallel_verify_time.stop();
        self.stats
            .votes_batch_parallel_verify_count
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .votes_batch_parallel_verify_elapsed_us
            .fetch_add(votes_batch_parallel_verify_time.as_us(), Ordering::Relaxed);
        verified_votes
    }

    fn verify_and_send_certificates(
        &self,
        certs_to_verify: Vec<Certificate>,
        bank: &Bank,
    ) -> Result<(), BLSSigVerifyServiceError> {
        let verified_certs = self.verify_certificates(certs_to_verify, bank);
        self.stats
            .total_valid_packets
            .fetch_add(verified_certs.len() as u64, Ordering::Relaxed);

        for cert in verified_certs {
            // Send the BLS certificate message to certificate pool.
            match self
                .message_sender
                .try_send(ConsensusMessage::Certificate(cert))
            {
                Ok(()) => {
                    self.stats.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    self.stats.sent_failed.fetch_add(1, Ordering::Relaxed);
                }
                Err(e @ TrySendError::Disconnected(_)) => {
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    fn verify_certificates(
        &self,
        certs_to_verify: Vec<Certificate>,
        bank: &Bank,
    ) -> Vec<Certificate> {
        if certs_to_verify.is_empty() {
            return vec![];
        }
        self.stats.certs_batch_count.fetch_add(1, Ordering::Relaxed);
        let mut certs_batch_verify_time = Measure::start("certs_batch_verify");
        let verified_certs = certs_to_verify
            .into_par_iter()
            .filter(
                |cert_to_verify| match self.verify_bls_certificate(cert_to_verify, bank) {
                    Ok(()) => true,
                    Err(e) => {
                        trace!(
                            "Failed to verify BLS certificate: {:?}, error: {e}",
                            cert_to_verify.cert_type
                        );
                        self.stats
                            .received_bad_signature_certs
                            .fetch_add(1, Ordering::Relaxed);
                        false
                    }
                },
            )
            .collect();
        certs_batch_verify_time.stop();
        self.stats
            .certs_batch_elapsed_us
            .fetch_add(certs_batch_verify_time.as_us(), Ordering::Relaxed);
        verified_certs
    }

    fn verify_bls_certificate(
        &self,
        cert_to_verify: &Certificate,
        bank: &Bank,
    ) -> Result<(), CertVerifyError> {
        if self
            .verified_certs
            .read()
            .unwrap()
            .contains(&cert_to_verify.cert_type)
        {
            self.stats.received_verified.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        let slot = cert_to_verify.cert_type.slot();
        let Some(key_to_rank_map) = get_key_to_rank_map(bank, slot) else {
            return Err(CertVerifyError::KeyToRankMapNotFound(slot));
        };

        let max_len = key_to_rank_map.len();

        let decoded_bitmap = match decode(&cert_to_verify.bitmap, max_len) {
            Ok(decoded) => decoded,
            Err(e) => {
                return Err(CertVerifyError::BitmapDecodingFailed(e));
            }
        };

        match decoded_bitmap {
            solana_signer_store::Decoded::Base2(bit_vec) => {
                self.verify_base2_certificate(cert_to_verify, &bit_vec, key_to_rank_map)?
            }
            solana_signer_store::Decoded::Base3(bit_vec1, bit_vec2) => self
                .verify_base3_certificate(cert_to_verify, &bit_vec1, &bit_vec2, key_to_rank_map)?,
        }

        self.verified_certs
            .write()
            .unwrap()
            .insert(cert_to_verify.cert_type);

        Ok(())
    }

    fn verify_base2_certificate(
        &self,
        cert_to_verify: &Certificate,
        bit_vec: &BitVec<u8, Lsb0>,
        key_to_rank_map: &Arc<BLSPubkeyToRankMap>,
    ) -> Result<(), CertVerifyError> {
        let original_vote = cert_to_verify.cert_type.to_source_vote(true).ok_or(
            CertVerifyError::NoSourceVoteFound(cert_to_verify.cert_type, true),
        )?;

        let Ok(signed_payload) = bincode::serialize(&original_vote) else {
            return Err(CertVerifyError::SerializationFailed);
        };

        let aggregate_bls_pubkey = aggregate_keys_from_bitmap(bit_vec, key_to_rank_map)?;

        if let Ok(true) =
            aggregate_bls_pubkey.verify_signature(&cert_to_verify.signature, &signed_payload)
        {
            Ok(())
        } else {
            Err(CertVerifyError::SignatureVerificationFailed)
        }
    }

    fn verify_base3_certificate(
        &self,
        cert_to_verify: &Certificate,
        bit_vec1: &BitVec<u8, Lsb0>,
        bit_vec2: &BitVec<u8, Lsb0>,
        key_to_rank_map: &Arc<BLSPubkeyToRankMap>,
    ) -> Result<(), CertVerifyError> {
        let vote1 = cert_to_verify.cert_type.to_source_vote(true).ok_or(
            CertVerifyError::NoSourceVoteFound(cert_to_verify.cert_type, true),
        )?;
        let vote2 = cert_to_verify.cert_type.to_source_vote(false).ok_or(
            CertVerifyError::NoSourceVoteFound(cert_to_verify.cert_type, false),
        )?;

        let Ok(signed_payload1) = bincode::serialize(&vote1) else {
            return Err(CertVerifyError::SerializationFailed);
        };
        let Ok(signed_payload2) = bincode::serialize(&vote2) else {
            return Err(CertVerifyError::SerializationFailed);
        };

        let messages_to_verify: Vec<&[u8]> = vec![&signed_payload1, &signed_payload2];

        // Aggregate the two sets of public keys separately from the two bitmaps.
        let agg_pk1 = aggregate_keys_from_bitmap(bit_vec1, key_to_rank_map)?;
        let agg_pk2 = aggregate_keys_from_bitmap(bit_vec2, key_to_rank_map)?;

        let pubkeys_affine: Vec<BlsPubkey> = vec![agg_pk1.into(), agg_pk2.into()];

        match SignatureProjective::par_verify_distinct_aggregated(
            &pubkeys_affine,
            &cert_to_verify.signature,
            &messages_to_verify,
        ) {
            Ok(true) => Ok(()),
            _ => Err(CertVerifyError::SignatureVerificationFailed),
        }
    }

    fn get_vote_payload(&self, vote: &Vote) -> Arc<Vec<u8>> {
        let read_cache = self.vote_payload_cache.read().unwrap();
        if let Some(payload) = read_cache.get(vote) {
            return payload.clone();
        }
        drop(read_cache);

        // Not in cache, so get a write lock
        let mut write_cache = self.vote_payload_cache.write().unwrap();
        if let Some(payload) = write_cache.get(vote) {
            return payload.clone();
        }

        let payload = Arc::new(bincode::serialize(vote).expect("Failed to serialize vote"));
        write_cache.insert(*vote, payload.clone());
        payload
    }
}

#[derive(Debug)]
struct VoteToVerify {
    vote_message: VoteMessage,
    bls_pubkey: BlsPubkey,
    pubkey: Pubkey,
}

// Add tests for the BLS signature verifier
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bls_sigverify::stats::STATS_INTERVAL_DURATION,
        agave_votor::consensus_pool::certificate_builder::CertificateBuilder,
        agave_votor_messages::{
            consensus_message::{
                Certificate, CertificateType, ConsensusMessage, VoteMessage,
                BLS_KEYPAIR_DERIVE_SEED,
            },
            vote::Vote,
        },
        rayon::ThreadPoolBuilder,
        solana_bls_signatures::{Keypair as BLSKeypair, Signature as BLSSignature},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_perf::packet::{Packet, RecycledPacketBatch},
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                create_genesis_config_with_alpenglow_vote_accounts, ValidatorVoteKeypairs,
            },
        },
        solana_signer::Signer,
        solana_signer_store::encode_base2,
        std::time::Instant,
    };

    fn create_keypairs_and_bls_sig_verifier(
        verfied_voter_slots_sender: VerifiedVoterSlotsSender,
        message_sender: Sender<ConsensusMessage>,
        consensus_metrics_sender: ConsensusMetricsEventSender,
    ) -> (Vec<ValidatorVoteKeypairs>, BLSSigVerifier) {
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let stakes_vec = (0..validator_keypairs.len())
            .map(|i| 1_000 - i as u64)
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            stakes_vec,
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank0);
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        (
            validator_keypairs,
            BLSSigVerifier::new(
                sharable_banks,
                verfied_voter_slots_sender,
                message_sender,
                consensus_metrics_sender,
            ),
        )
    }

    fn derive_bls_keypair(vote_keypair: &Keypair) -> BLSKeypair {
        BLSKeypair::derive_from_signer(vote_keypair, BLS_KEYPAIR_DERIVE_SEED).unwrap()
    }

    fn create_signed_vote_message(
        validator_keypairs: &[ValidatorVoteKeypairs],
        vote: Vote,
        rank: usize,
    ) -> VoteMessage {
        let bls_keypair = derive_bls_keypair(&validator_keypairs[rank].vote_keypair);
        let payload = bincode::serialize(&vote).expect("Failed to serialize vote");
        let signature: BLSSignature = bls_keypair.sign(&payload).into();
        VoteMessage {
            vote,
            signature,
            rank: rank as u16,
        }
    }

    fn create_signed_certificate_message(
        validator_keypairs: &[ValidatorVoteKeypairs],
        cert_type: CertificateType,
        ranks: &[usize],
    ) -> Certificate {
        let mut builder = CertificateBuilder::new(cert_type);
        // Assumes Base2 encoding (single vote type) for simplicity in this helper.
        let vote = cert_type.to_source_vote(true).unwrap();
        let vote_messages: Vec<VoteMessage> = ranks
            .iter()
            .map(|&rank| create_signed_vote_message(validator_keypairs, vote, rank))
            .collect();

        builder
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        builder.build().expect("Failed to build certificate")
    }

    #[test]
    fn test_blssigverifier_send_packets() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (verfied_voter_slots_sender, verfied_vote_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _consensus_metrics_receiver) =
            crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let vote_rank1 = 2;
        let cert_type = CertificateType::Finalize(4);
        let vote_message1 = create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            vote_rank1,
        );
        let cert = create_signed_certificate_message(&validator_keypairs, cert_type, &[vote_rank1]);
        let messages1 = vec![
            ConsensusMessage::Vote(vote_message1),
            ConsensusMessage::Certificate(cert),
        ];

        assert!(verifier
            .verify_and_send_batches(messages_to_batches(&messages1), Some(&pool))
            .is_ok());
        assert_eq!(receiver.try_iter().count(), 2);
        assert_eq!(verifier.stats.sent.load(Ordering::Relaxed), 2);
        assert_eq!(verifier.stats.received.load(Ordering::Relaxed), 2);
        let received_verified_votes1 = verfied_vote_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes1,
            (
                validator_keypairs[vote_rank1].vote_keypair.pubkey(),
                vec![5]
            )
        );

        let vote_rank2 = 3;
        let vote_message2 = create_signed_vote_message(
            &validator_keypairs,
            Vote::new_notarization_vote(6, Hash::new_unique()),
            vote_rank2,
        );
        let messages2 = vec![ConsensusMessage::Vote(vote_message2)];
        assert!(verifier
            .verify_and_send_batches(messages_to_batches(&messages2), Some(&pool))
            .is_ok());

        assert_eq!(receiver.try_iter().count(), 1);
        assert_eq!(verifier.stats.sent.load(Ordering::Relaxed), 3); // 2 + 1 = 3
        assert_eq!(verifier.stats.received.load(Ordering::Relaxed), 3); // 2 + 1 = 3
        let received_verified_votes2 = verfied_vote_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes2,
            (
                validator_keypairs[vote_rank2].vote_keypair.pubkey(),
                vec![6]
            )
        );

        verifier.stats.last_stats_logged = Instant::now() - STATS_INTERVAL_DURATION;
        let vote_rank3 = 9;
        let vote_message3 = create_signed_vote_message(
            &validator_keypairs,
            Vote::new_notarization_fallback_vote(7, Hash::new_unique()),
            vote_rank3,
        );
        let messages3 = vec![ConsensusMessage::Vote(vote_message3)];
        assert!(verifier
            .verify_and_send_batches(messages_to_batches(&messages3), Some(&pool))
            .is_ok());
        assert_eq!(receiver.try_iter().count(), 1);
        assert_eq!(verifier.stats.sent.load(Ordering::Relaxed), 0);
        assert_eq!(verifier.stats.received.load(Ordering::Relaxed), 0);
        let received_verified_votes3 = verfied_vote_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes3,
            (
                validator_keypairs[vote_rank3].vote_keypair.pubkey(),
                vec![7]
            )
        );
    }

    #[test]
    fn test_blssigverifier_verify_malformed() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let packets = vec![Packet::default()];
        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());

        assert_eq!(verifier.stats.received.load(Ordering::Relaxed), 1);
        assert_eq!(verifier.stats.received_malformed.load(Ordering::Relaxed), 1);

        // Expect no messages since the packet was malformed
        assert!(receiver.is_empty(), "Malformed packet should not be sent");

        // Send a packet with no epoch stakes
        let vote_message_no_stakes = create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5_000_000_000), // very high slot
            0,
        );
        let messages_no_stakes = vec![ConsensusMessage::Vote(vote_message_no_stakes)];

        assert!(verifier
            .verify_and_send_batches(messages_to_batches(&messages_no_stakes), Some(&pool))
            .is_ok());

        assert_eq!(
            verifier
                .stats
                .received_no_epoch_stakes
                .load(Ordering::Relaxed),
            1
        );

        // Expect no messages since the packet was malformed
        assert!(
            receiver.is_empty(),
            "Packet with no epoch stakes should not be sent"
        );

        // Send a packet with invalid rank
        let messages_invalid_rank = vec![ConsensusMessage::Vote(VoteMessage {
            vote: Vote::new_finalization_vote(5),
            signature: BLSSignature::default(),
            rank: 1000, // Invalid rank
        })];
        assert!(verifier
            .verify_and_send_batches(messages_to_batches(&messages_invalid_rank), Some(&pool))
            .is_ok());
        assert_eq!(verifier.stats.received_bad_rank.load(Ordering::Relaxed), 1);

        // Expect no messages since the packet was malformed
        assert!(
            receiver.is_empty(),
            "Packet with invalid rank should not be sent"
        );
    }

    #[test]
    fn test_blssigverifier_send_packets_channel_full() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let msg1 = ConsensusMessage::Vote(create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            0,
        ));
        let msg2 = ConsensusMessage::Vote(create_signed_vote_message(
            &validator_keypairs,
            Vote::new_notarization_fallback_vote(6, Hash::new_unique()),
            2,
        ));
        let messages = vec![msg1.clone(), msg2];
        assert!(verifier
            .verify_and_send_batches(messages_to_batches(&messages), Some(&pool))
            .is_ok());

        // We failed to send the second message because the channel is full.
        assert_eq!(receiver.len(), 1);
        assert_eq!(receiver.recv().unwrap(), msg1);
        assert_eq!(verifier.stats.sent.load(Ordering::Relaxed), 1);
        assert_eq!(verifier.stats.sent_failed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_blssigverifier_send_packets_receiver_closed() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        // Close the receiver to simulate a disconnected channel.
        drop(receiver);

        let msg = ConsensusMessage::Vote(create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            0,
        ));
        let messages = vec![msg];
        let result = verifier.verify_and_send_batches(messages_to_batches(&messages), Some(&pool));
        assert!(result.is_err());
    }

    #[test]
    fn test_blssigverifier_send_discarded_packets() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let message = ConsensusMessage::Vote(create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            0,
        ));
        let mut packet = Packet::default();
        packet
            .populate_packet(None, &message)
            .expect("Failed to populate packet");
        packet.meta_mut().set_discard(true); // Manually discard

        let packets = vec![packet];
        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];

        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        assert!(receiver.is_empty(), "Discarded packet should not be sent");
        assert_eq!(verifier.stats.sent.load(Ordering::Relaxed), 0);
        assert_eq!(verifier.stats.received.load(Ordering::Relaxed), 1);
        assert_eq!(verifier.stats.received_discarded.load(Ordering::Relaxed), 1);
        assert_eq!(verifier.stats.received_votes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_blssigverifier_verify_votes_all_valid() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let num_votes = 5;
        let mut packets = Vec::with_capacity(num_votes);
        let vote = Vote::new_skip_vote(42);

        for i in 0..num_votes {
            let consensus_message =
                ConsensusMessage::Vote(create_signed_vote_message(&validator_keypairs, vote, i));
            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        assert_eq!(
            message_receiver.try_iter().count(),
            num_votes,
            "Did not send all valid packets"
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_two_distinct_messages() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let num_votes_group1 = 3;
        let num_votes_group2 = 4;
        let num_votes = num_votes_group1 + num_votes_group2;
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let _vote1_payload = bincode::serialize(&vote1).expect("Failed to serialize vote");
        let vote2 = Vote::new_notarization_vote(43, Hash::new_unique());
        let _vote2_payload = bincode::serialize(&vote2).expect("Failed to serialize vote");

        // Group 1 votes
        for (i, _) in validator_keypairs.iter().enumerate().take(num_votes_group1) {
            let msg =
                ConsensusMessage::Vote(create_signed_vote_message(&validator_keypairs, vote1, i));
            let mut p = Packet::default();
            p.populate_packet(None, &msg).unwrap();
            packets.push(p);
        }

        // Group 2 votes
        for (i, _) in validator_keypairs
            .iter()
            .enumerate()
            .skip(num_votes_group1)
            .take(num_votes_group2)
        {
            let msg =
                ConsensusMessage::Vote(create_signed_vote_message(&validator_keypairs, vote2, i));
            let mut p = Packet::default();
            p.populate_packet(None, &msg).unwrap();
            packets.push(p);
        }

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        assert_eq!(
            message_receiver.try_iter().count(),
            num_votes,
            "Did not send all valid packets"
        );
        assert_eq!(
            verifier
                .stats
                .votes_batch_distinct_messages_count
                .load(Ordering::Relaxed),
            2
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_invalid_in_two_distinct_messages() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let num_votes = 5;
        let invalid_rank = 3; // This voter will sign vote 2 with an invalid signature.
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let vote1_payload = bincode::serialize(&vote1).expect("Failed to serialize vote");
        let vote2 = Vote::new_skip_vote(43);
        let vote2_payload = bincode::serialize(&vote2).expect("Failed to serialize vote");
        let invalid_payload =
            bincode::serialize(&Vote::new_skip_vote(99)).expect("Failed to serialize vote");

        for (i, validator_keypair) in validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = derive_bls_keypair(&validator_keypair.vote_keypair);

            // Split the votes: Ranks 0, 1 sign vote 1. Ranks 2, 3, 4 sign vote 2.
            let (vote, payload) = if i < 2 {
                (vote1, &vote1_payload)
            } else {
                (vote2, &vote2_payload)
            };

            let signature = if rank == invalid_rank {
                bls_keypair.sign(&invalid_payload).into() // Invalid signature
            } else {
                bls_keypair.sign(payload).into()
            };

            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        let sent_messages: Vec<_> = message_receiver.try_iter().collect();
        assert_eq!(
            sent_messages.len(),
            num_votes - 1,
            "Only valid votes should be sent"
        );
        assert!(!sent_messages.iter().any(|msg| {
            if let ConsensusMessage::Vote(vm) = msg {
                vm.vote == vote2 && vm.rank == invalid_rank
            } else {
                false
            }
        }));
        assert_eq!(
            verifier
                .stats
                .received_bad_signature_votes
                .load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_one_invalid_signature() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let num_votes = 5;
        let invalid_rank = 2;
        let mut packets = Vec::with_capacity(num_votes);
        let mut consensus_messages = Vec::with_capacity(num_votes); // ADDED: To hold messages for later comparison.

        let vote = Vote::new_skip_vote(42);
        let valid_vote_payload = bincode::serialize(&vote).expect("Failed to serialize vote");
        let invalid_vote_payload =
            bincode::serialize(&Vote::new_skip_vote(99)).expect("Failed to serialize vote");

        for (i, validator_keypair) in validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = derive_bls_keypair(&validator_keypair.vote_keypair);

            let signature = if rank == invalid_rank {
                bls_keypair.sign(&invalid_vote_payload).into() // Invalid signature
            } else {
                bls_keypair.sign(&valid_vote_payload).into() // Valid signature
            };

            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });

            consensus_messages.push(consensus_message.clone());

            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        let sent_messages: Vec<_> = message_receiver.try_iter().collect();
        assert_eq!(
            sent_messages.len(),
            num_votes - 1,
            "Only valid votes should be sent"
        );

        // Ensure the message with the invalid rank is not in the sent messages.
        assert!(!sent_messages.iter().any(|msg| {
            if let ConsensusMessage::Vote(vm) = msg {
                vm.rank == invalid_rank
            } else {
                false
            }
        }));

        assert_eq!(
            verifier
                .stats
                .received_bad_signature_votes
                .load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_empty_batch() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, _) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (_, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );

        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let packet_batches: Vec<PacketBatch> = vec![];
        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        assert_eq!(verifier.stats.received.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_verify_certificate_base2_valid() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let num_signers = 7; // > 2/3 of 10 validators
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert = create_signed_certificate_message(
            &validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        assert_eq!(
            message_receiver.try_iter().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_valid() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let slot = 20;
        let block_hash = Hash::new_unique();
        let notarize_vote = Vote::new_notarization_vote(slot, block_hash);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(slot, block_hash);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &validator_keypairs,
                notarize_vote,
                i,
            ))
        });
        (4..7).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &validator_keypairs,
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(slot, block_hash);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        assert_eq!(
            message_receiver.try_iter().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_invalid_signature() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (_validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let num_signers = 7;
        let slot = 10;
        let block_hash = Hash::new_unique();
        let cert_type = CertificateType::Notarize(slot, block_hash);
        let mut bitmap = BitVec::<u8, Lsb0>::new();
        bitmap.resize(num_signers, false);
        for i in 0..num_signers {
            bitmap.set(i, true);
        }
        let encoded_bitmap = encode_base2(&bitmap).unwrap();

        let cert = Certificate {
            cert_type,
            signature: BLSSignature::default(), // Use a default/wrong signature
            bitmap: encoded_bitmap,
        };
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        assert!(
            message_receiver.is_empty(),
            "Certificate with invalid signature should be discarded"
        );
        assert_eq!(
            verifier
                .stats
                .received_bad_signature_certs
                .load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn test_verify_mixed_valid_batch() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let mut packets = Vec::new();
        let num_votes = 2;

        for i in 0..num_votes {
            let consensus_message = ConsensusMessage::Vote(create_signed_vote_message(
                &validator_keypairs,
                Vote::new_skip_vote(42),
                i,
            ));
            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let num_cert_signers = 7;
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert_original_vote = Vote::new_notarization_vote(10, cert_type.to_block().unwrap().1);

        let cert_vote_messages: Vec<VoteMessage> = (0..num_cert_signers)
            .map(|i| create_signed_vote_message(&validator_keypairs, cert_original_vote, i))
            .collect();
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&cert_vote_messages)
            .expect("Failed to aggregate votes for certificate");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        let mut cert_packet = Packet::default();
        cert_packet
            .populate_packet(None, &consensus_message_cert)
            .unwrap();
        packets.push(cert_packet);

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        assert_eq!(
            message_receiver.try_iter().count(),
            num_votes + 1,
            "All valid messages in a mixed batch should be sent"
        );
        assert_eq!(
            verifier.stats.sent.load(Ordering::Relaxed),
            (num_votes + 1) as u64
        );
    }

    #[test]
    fn test_verify_vote_with_invalid_rank() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let invalid_rank = 999;
        let vote = Vote::new_skip_vote(42);
        let vote_payload = bincode::serialize(&vote).unwrap();
        let bls_keypair = derive_bls_keypair(&validator_keypairs[0].vote_keypair);
        let signature: BLSSignature = bls_keypair.sign(&vote_payload).into();

        let consensus_message = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: invalid_rank,
        });

        let packet_batches = messages_to_batches(&[consensus_message]);
        assert!(verifier
            .verify_and_send_batches(packet_batches, Some(&pool))
            .is_ok());
        assert!(
            message_receiver.is_empty(),
            "Packet with invalid rank should be discarded"
        );
        assert_eq!(verifier.stats.received_bad_rank.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_verify_old_vote_and_cert() {
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let stakes_vec = (0..validator_keypairs.len())
            .map(|i| 1_000 - i as u64)
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            stakes_vec,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let bank5 = Bank::new_from_parent(Arc::new(bank0), &Pubkey::default(), 5);
        let bank_forks = BankForks::new_rw_arc(bank5);

        bank_forks.write().unwrap().set_root(5, None, None);

        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let mut sig_verifier = BLSSigVerifier::new(
            sharable_banks,
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );

        let vote = Vote::new_skip_vote(2);
        let consensus_message_vote =
            ConsensusMessage::Vote(create_signed_vote_message(&validator_keypairs, vote, 0));
        let packet_batches_vote = messages_to_batches(&[consensus_message_vote]);

        assert!(sig_verifier
            .verify_and_send_batches(packet_batches_vote, Some(&pool))
            .is_ok());
        assert!(
            message_receiver.is_empty(),
            "Old vote should not have been sent"
        );
        assert_eq!(sig_verifier.stats.received_old.load(Ordering::Relaxed), 1);

        let cert = create_signed_certificate_message(
            &validator_keypairs,
            CertificateType::Finalize(3),
            &[0], // Signer rank 0
        );
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        let packet_batches_cert = messages_to_batches(&[consensus_message_cert]);

        assert!(sig_verifier
            .verify_and_send_batches(packet_batches_cert, Some(&pool))
            .is_ok());
        assert!(
            message_receiver.is_empty(),
            "Old certificate should not have been sent"
        );
        assert_eq!(sig_verifier.stats.received_old.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_verified_certs_are_skipped() {
        let (verfied_voter_slots_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier(
            verfied_voter_slots_sender,
            message_sender,
            consensus_metrics_sender,
        );
        let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();

        let num_signers = 8;
        let slot = 10;
        let block_hash = Hash::new_unique();
        let cert_type = CertificateType::Notarize(slot, block_hash);
        let original_vote = Vote::new_notarization_vote(slot, block_hash);
        let mut vote_messages: Vec<VoteMessage> = (0..num_signers)
            .map(|i| create_signed_vote_message(&validator_keypairs, original_vote, i))
            .collect();

        let mut builder1 = CertificateBuilder::new(cert_type);
        builder1
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert1 = builder1.build().expect("Failed to build certificate");
        let consensus_message1 = ConsensusMessage::Certificate(cert1);
        let packet_batches1 = messages_to_batches(&[consensus_message1]);

        assert!(verifier
            .verify_and_send_batches(packet_batches1, Some(&pool))
            .is_ok());

        assert_eq!(
            message_receiver.try_iter().count(),
            1,
            "First certificate should be sent"
        );
        assert_eq!(verifier.stats.received_verified.load(Ordering::Relaxed), 0);

        vote_messages.pop(); // Remove one signature
        let mut builder2 = CertificateBuilder::new(cert_type);
        builder2
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert2 = builder2.build().expect("Failed to build certificate");
        let consensus_message2 = ConsensusMessage::Certificate(cert2);
        let packet_batches2 = messages_to_batches(&[consensus_message2]);

        assert!(verifier
            .verify_and_send_batches(packet_batches2, Some(&pool))
            .is_ok());
        assert!(
            message_receiver.is_empty(),
            "Second, weaker certificate should not be sent"
        );
        assert_eq!(
            verifier.stats.received.load(Ordering::Relaxed),
            2,
            "Should have received two packets in total"
        );
        assert_eq!(
            verifier.stats.received_verified.load(Ordering::Relaxed),
            1,
            "Should have detected one already-verified cert"
        );
    }

    fn messages_to_batches(messages: &[ConsensusMessage]) -> Vec<PacketBatch> {
        let packets: Vec<_> = messages
            .iter()
            .map(|msg| {
                let mut p = Packet::default();
                p.populate_packet(None, msg).unwrap();
                p
            })
            .collect();
        vec![RecycledPacketBatch::new(packets).into()]
    }
}
