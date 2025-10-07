//! The BLS signature verifier.
#![allow(unused_imports)]

use {
    crate::{
        bls_sigverify::{
            bls_sigverify_stage::BLSSigVerifyServiceError, stats::BLSSigVerifierStats,
        },
        cluster_info_vote_listener::VerifiedVoteSender,
    },
    bitvec::prelude::{BitVec, Lsb0},
    crossbeam_channel::{Sender, TrySendError},
    rayon::iter::{
        IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
    },
    solana_bls_signatures::{
        pubkey::{Pubkey as BlsPubkey, PubkeyProjective, VerifiablePubkey},
        signature::{Signature as BlsSignature, SignatureProjective},
    },
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_perf::packet::PacketRefMut,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks, epoch_stakes::BLSPubkeyToRankMap},
    solana_signer_store::{decode, DecodeError},
    solana_streamer::packet::PacketBatch,
    solana_votor_messages::{
        consensus_message::{
            Certificate, CertificateMessage, CertificateType, ConsensusMessage, VoteMessage,
        },
        vote::Vote,
    },
    std::{
        collections::{HashMap, HashSet},
        sync::{atomic::Ordering, Arc, RwLock},
    },
    thiserror::Error,
};

// TODO(sam): This logic of extracting the message payload for signature verification
//            is brittle, but another bincode serialization would be wasteful.
//            Revisit this to figure out the best way to handle this.

fn get_key_to_rank_map(bank: &Bank, slot: Slot) -> Option<&Arc<BLSPubkeyToRankMap>> {
    let stakes = bank.epoch_stakes_map();
    let epoch = bank.epoch_schedule().get_epoch(slot);
    stakes
        .get(&epoch)
        .map(|stake| stake.bls_pubkey_to_rank_map())
}

fn aggregate_keys_from_bitmap(
    bit_vec: &BitVec<u8, Lsb0>,
    key_to_rank_map: &Arc<BLSPubkeyToRankMap>,
) -> Option<PubkeyProjective> {
    let pubkeys: Result<Vec<_>, _> = bit_vec
        .iter_ones()
        .filter_map(|rank| key_to_rank_map.get_pubkey(rank))
        .map(|(_, bls_pubkey)| PubkeyProjective::try_from(*bls_pubkey))
        .collect();
    let pubkeys = pubkeys.ok()?;
    PubkeyProjective::par_aggregate(&pubkeys.iter().collect::<Vec<_>>()).ok()
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

    #[error("Base 3 encoding on unexpected cert {0:?}")]
    Base3EncodingOnUnexpectedCert(CertificateType),
}

pub struct BLSSigVerifier {
    verified_votes_sender: VerifiedVoteSender,
    message_sender: Sender<ConsensusMessage>,
    sharable_banks: SharableBanks,
    stats: BLSSigVerifierStats,
    verified_certs: RwLock<HashSet<Certificate>>,
    vote_payload_cache: RwLock<HashMap<Vote, Arc<Vec<u8>>>>,
}

impl BLSSigVerifier {
    pub fn verify_and_send_batches(
        &mut self,
        mut batches: Vec<PacketBatch>,
    ) -> Result<(), BLSSigVerifyServiceError<ConsensusMessage>> {
        let mut preprocess_time = Measure::start("preprocess");
        // TODO(sam): ideally we want to avoid heap allocation, but let's use
        //            `Vec` for now for clarity and then optimize for the final version
        let mut votes_to_verify = Vec::new();
        let mut certs_to_verify = Vec::new();

        let root_bank = self.sharable_banks.root();
        self.verified_certs
            .write()
            .unwrap()
            .retain(|cert| cert.slot() > root_bank.slot());
        self.vote_payload_cache
            .write()
            .unwrap()
            .retain(|vote, _| vote.slot() > root_bank.slot());

        for mut packet in batches.iter_mut().flatten() {
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
                    // Only need votes newer than root slot
                    if vote_message.vote.slot() <= root_bank.slot() {
                        self.stats.received_old.fetch_add(1, Ordering::Relaxed);
                        packet.meta_mut().set_discard(true);
                        continue;
                    }

                    // Missing epoch states
                    let Some(key_to_rank_map) =
                        get_key_to_rank_map(&root_bank, vote_message.vote.slot())
                    else {
                        self.stats
                            .received_no_epoch_stakes
                            .fetch_add(1, Ordering::Relaxed);
                        packet.meta_mut().set_discard(true);
                        continue;
                    };

                    // Invalid rank
                    let Some((solana_pubkey, bls_pubkey)) =
                        key_to_rank_map.get_pubkey(vote_message.rank.into())
                    else {
                        self.stats.received_bad_rank.fetch_add(1, Ordering::Relaxed);
                        packet.meta_mut().set_discard(true);
                        continue;
                    };

                    votes_to_verify.push(VoteToVerify {
                        vote_message,
                        bls_pubkey: *bls_pubkey,
                        pubkey: *solana_pubkey,
                    });
                }
                ConsensusMessage::Certificate(cert_message) => {
                    // Only need certs newer than root slot
                    if cert_message.certificate.slot() <= root_bank.slot() {
                        self.stats.received_old.fetch_add(1, Ordering::Relaxed);
                        packet.meta_mut().set_discard(true);
                        continue;
                    }

                    if self
                        .verified_certs
                        .read()
                        .unwrap()
                        .contains(&cert_message.certificate)
                    {
                        self.stats.received_verified.fetch_add(1, Ordering::Relaxed);
                        packet.meta_mut().set_discard(true);
                        continue;
                    }

                    certs_to_verify.push(CertToVerify { cert_message });
                }
            }
        }
        preprocess_time.stop();
        self.stats.preprocess_count.fetch_add(1, Ordering::Relaxed);
        self.stats
            .preprocess_elapsed_us
            .fetch_add(preprocess_time.as_us(), Ordering::Relaxed);

        let (votes_result, certs_result) = rayon::join(
            || self.verify_and_send_votes(votes_to_verify),
            || self.verify_and_send_certificates(certs_to_verify, &root_bank),
        );

        votes_result?;
        certs_result?;

        self.stats.maybe_report_stats();

        Ok(())
    }
}

impl BLSSigVerifier {
    pub fn new(
        sharable_banks: SharableBanks,
        verified_votes_sender: VerifiedVoteSender,
        message_sender: Sender<ConsensusMessage>,
    ) -> Self {
        Self {
            sharable_banks,
            verified_votes_sender,
            message_sender,
            stats: BLSSigVerifierStats::new(),
            verified_certs: RwLock::new(HashSet::new()),
            vote_payload_cache: RwLock::new(HashMap::new()),
        }
    }

    fn verify_and_send_votes(
        &self,
        votes_to_verify: Vec<VoteToVerify>,
    ) -> Result<(), BLSSigVerifyServiceError<ConsensusMessage>> {
        let verified_votes = self.verify_votes(votes_to_verify);
        self.stats
            .total_valid_packets
            .fetch_add(verified_votes.len() as u64, Ordering::Relaxed);

        let mut verified_votes_by_pubkey: HashMap<Pubkey, Vec<Slot>> = HashMap::new();
        for vote in verified_votes {
            self.stats.received_votes.fetch_add(1, Ordering::Relaxed);
            if vote.vote_message.vote.is_notarization_or_finalization()
                || vote.vote_message.vote.is_notarize_fallback()
            {
                let slot = vote.vote_message.vote.slot();
                let cur_slots: &mut Vec<Slot> =
                    verified_votes_by_pubkey.entry(vote.pubkey).or_default();
                if !cur_slots.contains(&slot) {
                    cur_slots.push(slot);
                }
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
            match self.verified_votes_sender.try_send((pubkey, slots)) {
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

    fn verify_votes(&self, votes_to_verify: Vec<VoteToVerify>) -> Vec<VoteToVerify> {
        if votes_to_verify.is_empty() {
            return vec![];
        }

        self.stats.votes_batch_count.fetch_add(1, Ordering::Relaxed);
        let mut votes_batch_optimistic_time = Measure::start("votes_batch_optimistic");
        let payloads: Vec<Arc<Vec<u8>>> = votes_to_verify
            .iter()
            .map(|v| self.get_vote_payload(&v.vote_message.vote))
            .collect();

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

        let distinct_data: Vec<_> = grouped_pubkeys.into_iter().collect();
        let aggregate_pubkeys_result: Result<Vec<PubkeyProjective>, _> = distinct_data
            .iter()
            .map(|(_, pubkeys)| PubkeyProjective::par_aggregate(pubkeys))
            .collect();

        let verified_optimistically = if let Ok(aggregate_pubkeys) = aggregate_pubkeys_result {
            let all_signatures: Vec<&BlsSignature> = votes_to_verify
                .iter()
                .map(|v| &v.vote_message.signature)
                .collect();

            if let Ok(aggregate_signature) = SignatureProjective::par_aggregate(&all_signatures) {
                if distinct_messages == 1 {
                    let payload_slice = distinct_data[0].0.as_slice();
                    aggregate_pubkeys[0]
                        .verify_signature(&aggregate_signature, payload_slice)
                        .unwrap_or(false)
                } else {
                    let payload_slices: Vec<&[u8]> =
                        distinct_data.iter().map(|(p, _)| p.as_slice()).collect();

                    let aggregate_pubkeys_affine: Vec<BlsPubkey> =
                        aggregate_pubkeys.into_iter().map(|pk| pk.into()).collect();
                    let aggregate_pubkeys_refs: Vec<&BlsPubkey> =
                        aggregate_pubkeys_affine.iter().collect();

                    SignatureProjective::par_verify_distinct_aggregated(
                        &aggregate_pubkeys_refs,
                        &aggregate_signature.into(),
                        &payload_slices,
                    )
                    .unwrap_or(false)
                }
            } else {
                false
            }
        } else {
            // Public key aggregation failed.
            false
        };

        if verified_optimistically {
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
                    .verify_signature(&vote_to_verify.vote_message.signature, payload)
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
            .map(|(vote, _payload)| vote)
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
        certs_to_verify: Vec<CertToVerify>,
        bank: &Bank,
    ) -> Result<(), BLSSigVerifyServiceError<ConsensusMessage>> {
        let verified_certs = self.verify_certificates(certs_to_verify, bank);
        self.stats
            .total_valid_packets
            .fetch_add(verified_certs.len() as u64, Ordering::Relaxed);

        for cert in verified_certs {
            // Send the BLS certificate message to certificate pool.
            match self
                .message_sender
                .try_send(ConsensusMessage::Certificate(cert.cert_message))
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
        certs_to_verify: Vec<CertToVerify>,
        bank: &Bank,
    ) -> Vec<CertToVerify> {
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
                            cert_to_verify.cert_message.certificate
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
        cert_to_verify: &CertToVerify,
        bank: &Bank,
    ) -> Result<(), CertVerifyError> {
        if self
            .verified_certs
            .read()
            .unwrap()
            .contains(&cert_to_verify.cert_message.certificate)
        {
            self.stats.received_verified.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        let slot = cert_to_verify.cert_message.certificate.slot();
        let Some(key_to_rank_map) = get_key_to_rank_map(bank, slot) else {
            return Err(CertVerifyError::KeyToRankMapNotFound(slot));
        };

        let max_len = key_to_rank_map.len();

        let decoded_bitmap = match decode(&cert_to_verify.cert_message.bitmap, max_len) {
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
            .insert(cert_to_verify.cert_message.certificate);

        Ok(())
    }

    fn verify_base2_certificate(
        &self,
        cert_to_verify: &CertToVerify,
        bit_vec: &BitVec<u8, Lsb0>,
        key_to_rank_map: &Arc<BLSPubkeyToRankMap>,
    ) -> Result<(), CertVerifyError> {
        let original_vote = cert_to_verify.cert_message.certificate.to_source_vote();

        let Ok(signed_payload) = bincode::serialize(&original_vote) else {
            return Err(CertVerifyError::SerializationFailed);
        };

        let Some(aggregate_bls_pubkey) = aggregate_keys_from_bitmap(bit_vec, key_to_rank_map)
        else {
            return Err(CertVerifyError::KeyAggregationFailed);
        };

        if let Ok(true) = aggregate_bls_pubkey
            .verify_signature(&cert_to_verify.cert_message.signature, &signed_payload)
        {
            Ok(())
        } else {
            Err(CertVerifyError::SignatureVerificationFailed)
        }
    }

    fn verify_base3_certificate(
        &self,
        cert_to_verify: &CertToVerify,
        bit_vec1: &BitVec<u8, Lsb0>,
        bit_vec2: &BitVec<u8, Lsb0>,
        key_to_rank_map: &Arc<BLSPubkeyToRankMap>,
    ) -> Result<(), CertVerifyError> {
        let Some((vote1, vote2)) = cert_to_verify.cert_message.certificate.to_source_votes() else {
            return Err(CertVerifyError::Base3EncodingOnUnexpectedCert(
                cert_to_verify.cert_message.certificate.certificate_type(),
            ));
        };

        let Ok(signed_payload1) = bincode::serialize(&vote1) else {
            return Err(CertVerifyError::SerializationFailed);
        };
        let Ok(signed_payload2) = bincode::serialize(&vote2) else {
            return Err(CertVerifyError::SerializationFailed);
        };

        let messages_to_verify: Vec<&[u8]> = vec![&signed_payload1, &signed_payload2];

        // Aggregate the two sets of public keys separately from the two bitmaps.
        let Some(agg_pk1) = aggregate_keys_from_bitmap(bit_vec1, key_to_rank_map) else {
            return Err(CertVerifyError::KeyAggregationFailed);
        };
        let Some(agg_pk2) = aggregate_keys_from_bitmap(bit_vec2, key_to_rank_map) else {
            return Err(CertVerifyError::KeyAggregationFailed);
        };

        let pubkeys_affine: Vec<BlsPubkey> = vec![agg_pk1.into(), agg_pk2.into()];
        let pubkey_refs: Vec<&BlsPubkey> = pubkeys_affine.iter().collect();

        match SignatureProjective::par_verify_distinct_aggregated(
            &pubkey_refs,
            &cert_to_verify.cert_message.signature,
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

struct CertToVerify {
    cert_message: CertificateMessage,
}

// Add tests for the BLS signature verifier
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bls_sigverify::stats::STATS_INTERVAL_DURATION,
        crossbeam_channel::Receiver,
        solana_bls_signatures::{Signature, Signature as BLSSignature},
        solana_hash::Hash,
        solana_perf::packet::{Packet, PinnedPacketBatch},
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                create_genesis_config_with_alpenglow_vote_accounts, ValidatorVoteKeypairs,
            },
        },
        solana_signer::Signer,
        solana_signer_store::encode_base2,
        solana_votor::consensus_pool::vote_certificate_builder::VoteCertificateBuilder,
        solana_votor_messages::{
            consensus_message::{
                Certificate, CertificateMessage, CertificateType, ConsensusMessage, VoteMessage,
            },
            vote::Vote,
        },
        std::time::{Duration, Instant},
    };

    fn create_keypairs_and_bls_sig_verifier(
        verified_vote_sender: VerifiedVoteSender,
        message_sender: Sender<ConsensusMessage>,
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
            BLSSigVerifier::new(sharable_banks, verified_vote_sender, message_sender),
        )
    }

    fn create_signed_vote_message(
        validator_keypairs: &[ValidatorVoteKeypairs],
        vote: Vote,
        rank: usize,
    ) -> VoteMessage {
        let bls_keypair = &validator_keypairs[rank].bls_keypair;
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
        certificate: Certificate,
        ranks: &[usize],
    ) -> CertificateMessage {
        let mut builder = VoteCertificateBuilder::new(certificate);
        // Assumes Base2 encoding (single vote type) for simplicity in this helper.
        let vote = certificate.to_source_vote();
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
        let (verified_vote_sender, verfied_vote_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);

        let vote_rank1 = 2;
        let certificate = Certificate::new(CertificateType::Finalize, 4, None);
        let vote_message1 = create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            vote_rank1,
        );
        let cert_message =
            create_signed_certificate_message(&validator_keypairs, certificate, &[vote_rank1]);
        let messages1 = vec![
            ConsensusMessage::Vote(vote_message1),
            ConsensusMessage::Certificate(cert_message),
        ];

        assert!(verifier
            .verify_and_send_batches(messages_to_batches(&messages1))
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
            .verify_and_send_batches(messages_to_batches(&messages2))
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
            .verify_and_send_batches(messages_to_batches(&messages3))
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
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);

        let packets = vec![Packet::default()];
        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];
        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());

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
            .verify_and_send_batches(messages_to_batches(&messages_no_stakes))
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
            signature: Signature::default(),
            rank: 1000, // Invalid rank
        })];
        assert!(verifier
            .verify_and_send_batches(messages_to_batches(&messages_invalid_rank))
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
        solana_logger::setup();
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);

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
            .verify_and_send_batches(messages_to_batches(&messages))
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
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);

        // Close the receiver to simulate a disconnected channel.
        drop(receiver);

        let msg = ConsensusMessage::Vote(create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            0,
        ));
        let messages = vec![msg];
        let result = verifier.verify_and_send_batches(messages_to_batches(&messages));
        assert!(result.is_err());
    }

    #[test]
    fn test_blssigverifier_send_discarded_packets() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);

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
        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];

        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
        assert!(receiver.is_empty(), "Discarded packet should not be sent");
        assert_eq!(verifier.stats.sent.load(Ordering::Relaxed), 0);
        assert_eq!(verifier.stats.received.load(Ordering::Relaxed), 1);
        assert_eq!(verifier.stats.received_discarded.load(Ordering::Relaxed), 1);
        assert_eq!(verifier.stats.received_votes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_blssigverifier_verify_votes_all_valid() {
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

        let num_votes = 5;
        let mut packets = Vec::with_capacity(num_votes);
        let vote = Vote::new_skip_vote(42);
        let vote_payload = bincode::serialize(&vote).expect("Failed to serialize vote");

        for (i, validator_keypair) in validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature: BLSSignature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];
        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
        assert_eq!(
            message_receiver.try_iter().count(),
            num_votes,
            "Did not send all valid packets"
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_two_distinct_messages() {
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

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

        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];
        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
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
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

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
            let bls_keypair = &validator_keypair.bls_keypair;

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

        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];
        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
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
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

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
            let bls_keypair = &validator_keypair.bls_keypair;

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

        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];
        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
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
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, _) = crossbeam_channel::unbounded();
        let (_, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

        let packet_batches: Vec<PacketBatch> = vec![];
        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
        assert_eq!(verifier.stats.received.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_verify_certificate_base2_valid() {
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

        let num_signers = 7; // > 2/3 of 10 validators
        let certificate = Certificate::Notarize(10, Hash::new_unique());
        let cert_message = create_signed_certificate_message(
            &validator_keypairs,
            certificate,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert_message);
        let packet_batches = messages_to_batches(&[consensus_message]);

        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
        assert_eq!(
            message_receiver.try_iter().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_valid() {
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

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
        let certificate = Certificate::NotarizeFallback(slot, block_hash);
        let mut builder = VoteCertificateBuilder::new(certificate);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert_message = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert_message);
        let packet_batches = messages_to_batches(&[consensus_message]);

        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
        assert_eq!(
            message_receiver.try_iter().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_invalid_signature() {
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (_validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

        let num_signers = 7;
        let slot = 10;
        let block_hash = Hash::new_unique();
        let certificate = Certificate::Notarize(slot, block_hash);
        let mut bitmap = BitVec::<u8, Lsb0>::new();
        bitmap.resize(num_signers, false);
        for i in 0..num_signers {
            bitmap.set(i, true);
        }
        let encoded_bitmap = encode_base2(&bitmap).unwrap();

        let cert_message = CertificateMessage {
            certificate,
            signature: BLSSignature::default(), // Use a default/wrong signature
            bitmap: encoded_bitmap,
        };
        let consensus_message = ConsensusMessage::Certificate(cert_message);
        let packet_batches = messages_to_batches(&[consensus_message]);

        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
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
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

        let mut packets = Vec::new();
        let num_votes = 2;

        let vote = Vote::new_skip_vote(42);
        let vote_payload = bincode::serialize(&vote).unwrap();
        for (i, validator_keypair) in validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature: BLSSignature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let num_cert_signers = 7;
        let certificate = Certificate::Notarize(10, Hash::new_unique());
        let cert_original_vote = Vote::new_notarization_vote(10, certificate.to_block().unwrap().1);
        let cert_payload = bincode::serialize(&cert_original_vote).unwrap();

        let cert_vote_messages: Vec<VoteMessage> = (0..num_cert_signers)
            .map(|i| {
                let signature = validator_keypairs[i].bls_keypair.sign(&cert_payload);
                VoteMessage {
                    vote: cert_original_vote,
                    signature: signature.into(),
                    rank: i as u16,
                }
            })
            .collect();
        let mut builder =
            solana_votor::consensus_pool::vote_certificate_builder::VoteCertificateBuilder::new(
                certificate,
            );
        builder
            .aggregate(&cert_vote_messages)
            .expect("Failed to aggregate votes for certificate");
        let cert_message = builder.build().expect("Failed to build certificate");
        let consensus_message_cert = ConsensusMessage::Certificate(cert_message);
        let mut cert_packet = Packet::default();
        cert_packet
            .populate_packet(None, &consensus_message_cert)
            .unwrap();
        packets.push(cert_packet);

        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];
        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
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
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

        let invalid_rank = 999;
        let vote = Vote::new_skip_vote(42);
        let vote_payload = bincode::serialize(&vote).unwrap();
        let bls_keypair = &validator_keypairs[0].bls_keypair;
        let signature: BLSSignature = bls_keypair.sign(&vote_payload).into();

        let consensus_message = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: invalid_rank,
        });

        let packet_batches = messages_to_batches(&[consensus_message]);
        assert!(verifier.verify_and_send_batches(packet_batches).is_ok());
        assert!(
            message_receiver.is_empty(),
            "Packet with invalid rank should be discarded"
        );
        assert_eq!(verifier.stats.received_bad_rank.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_verify_old_vote_and_cert() {
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
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
        let bank5 = Bank::new_from_parent(Arc::new(bank0), &Pubkey::default(), 5);
        let bank_forks = BankForks::new_rw_arc(bank5);

        bank_forks.write().unwrap().set_root(5, None, None).unwrap();

        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let mut sig_verifier =
            BLSSigVerifier::new(sharable_banks, verified_vote_sender, message_sender);

        let vote = Vote::new_skip_vote(2);
        let vote_payload = bincode::serialize(&vote).unwrap();
        let bls_keypair = &validator_keypairs[0].bls_keypair;
        let signature: BLSSignature = bls_keypair.sign(&vote_payload).into();
        let consensus_message_vote = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: 0,
        });
        let packet_batches_vote = messages_to_batches(&[consensus_message_vote]);

        assert!(sig_verifier
            .verify_and_send_batches(packet_batches_vote)
            .is_ok());
        assert!(
            message_receiver.is_empty(),
            "Old vote should not have been sent"
        );
        assert_eq!(sig_verifier.stats.received_old.load(Ordering::Relaxed), 1);

        let cert_message = create_signed_certificate_message(
            &validator_keypairs,
            Certificate::new(CertificateType::Finalize, 3, None),
            &[0], // Signer rank 0
        );
        let consensus_message_cert = ConsensusMessage::Certificate(cert_message);
        let packet_batches_cert = messages_to_batches(&[consensus_message_cert]);

        assert!(sig_verifier
            .verify_and_send_batches(packet_batches_cert)
            .is_ok());
        assert!(
            message_receiver.is_empty(),
            "Old certificate should not have been sent"
        );
        assert_eq!(sig_verifier.stats.received_old.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_verified_certs_are_skipped() {
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, message_sender);

        let num_signers = 8;
        let slot = 10;
        let block_hash = Hash::new_unique();
        let certificate = Certificate::Notarize(slot, block_hash);
        let original_vote = Vote::new_notarization_vote(slot, block_hash);
        let signed_payload = bincode::serialize(&original_vote).unwrap();
        let mut vote_messages: Vec<VoteMessage> = (0..num_signers)
            .map(|i| {
                let signature = validator_keypairs[i].bls_keypair.sign(&signed_payload);
                VoteMessage {
                    vote: original_vote,
                    signature: signature.into(),
                    rank: i as u16,
                }
            })
            .collect();

        let mut builder1 = VoteCertificateBuilder::new(certificate);
        builder1
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert_message1 = builder1.build().expect("Failed to build certificate");
        let consensus_message1 = ConsensusMessage::Certificate(cert_message1);
        let packet_batches1 = messages_to_batches(&[consensus_message1]);

        assert!(verifier.verify_and_send_batches(packet_batches1).is_ok());

        assert_eq!(
            message_receiver.try_iter().count(),
            1,
            "First certificate should be sent"
        );
        assert_eq!(verifier.stats.received_verified.load(Ordering::Relaxed), 0);

        vote_messages.pop(); // Remove one signature
        let mut builder2 = VoteCertificateBuilder::new(certificate);
        builder2
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert_message2 = builder2.build().expect("Failed to build certificate");
        let consensus_message2 = ConsensusMessage::Certificate(cert_message2);
        let packet_batches2 = messages_to_batches(&[consensus_message2]);

        assert!(verifier.verify_and_send_batches(packet_batches2).is_ok());
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
        vec![PinnedPacketBatch::new(packets).into()]
    }
}
