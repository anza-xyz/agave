//! The BLS signature verifier.
//! This is just a placeholder for now, until we have a real implementation.

mod stats;

use {
    crate::{
        cluster_info_vote_listener::VerifiedVoteSender,
        sigverify_stage::{SigVerifier, SigVerifyServiceError},
    },
    crossbeam_channel::{Sender, TrySendError},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank, epoch_stakes::BLSPubkeyToRankMap, root_bank_cache::RootBankCache,
    },
    solana_streamer::packet::PacketBatch,
    solana_votor_messages::bls_message::BLSMessage,
    stats::{BLSSigVerifierStats, StatsUpdater},
    std::{collections::HashMap, sync::Arc},
};

fn get_key_to_rank_map(bank: &Bank, slot: Slot) -> Option<&Arc<BLSPubkeyToRankMap>> {
    let stakes = bank.epoch_stakes_map();
    let epoch = bank.epoch_schedule().get_epoch(slot);
    stakes
        .get(&epoch)
        .map(|stake| stake.bls_pubkey_to_rank_map())
}

pub struct BLSSigVerifier {
    verified_votes_sender: VerifiedVoteSender,
    message_sender: Sender<BLSMessage>,
    root_bank_cache: RootBankCache,
    stats: BLSSigVerifierStats,
}

impl SigVerifier for BLSSigVerifier {
    type SendType = BLSMessage;

    // TODO(wen): just a placeholder without any verification.
    fn verify_batches(&self, batches: Vec<PacketBatch>, _valid_packets: usize) -> Vec<PacketBatch> {
        batches
    }

    fn send_packets(
        &mut self,
        packet_batches: Vec<PacketBatch>,
    ) -> Result<(), SigVerifyServiceError<Self::SendType>> {
        // TODO(wen): just a placeholder without any batching.
        let mut verified_votes = HashMap::new();
        let mut stats_updater = StatsUpdater::default();

        for packet in packet_batches.iter().flatten() {
            stats_updater.received += 1;

            if packet.meta().discard() {
                stats_updater.received_discarded += 1;
                continue;
            }

            let message = match packet.deserialize_slice(..) {
                Ok(msg) => msg,
                Err(e) => {
                    trace!("Failed to deserialize BLS message: {}", e);
                    stats_updater.received_malformed += 1;
                    continue;
                }
            };

            let slot = match &message {
                BLSMessage::Vote(vote_message) => vote_message.vote.slot(),
                BLSMessage::Certificate(certificate_message) => {
                    certificate_message.certificate.slot()
                }
            };

            let bank = self.root_bank_cache.root_bank();
            let Some(rank_to_pubkey_map) = get_key_to_rank_map(&bank, slot) else {
                stats_updater.received_no_epoch_stakes += 1;
                continue;
            };

            if let BLSMessage::Vote(vote_message) = &message {
                let vote = &vote_message.vote;
                stats_updater.received_votes += 1;
                if vote.is_notarization_or_finalization() || vote.is_notarize_fallback() {
                    let Some((pubkey, _)) = rank_to_pubkey_map.get_pubkey(vote_message.rank.into())
                    else {
                        stats_updater.received_malformed += 1;
                        continue;
                    };
                    let cur_slots: &mut Vec<Slot> = verified_votes.entry(*pubkey).or_default();
                    if !cur_slots.contains(&slot) {
                        cur_slots.push(slot);
                    }
                }
            }

            // Now send the BLS message to certificate pool.
            match self.message_sender.try_send(message) {
                Ok(()) => stats_updater.sent += 1,
                Err(TrySendError::Full(_)) => {
                    stats_updater.sent_failed += 1;
                }
                Err(e @ TrySendError::Disconnected(_)) => {
                    return Err(e.into());
                }
            }
        }
        self.send_verified_votes(verified_votes);
        self.stats.update(stats_updater);
        self.stats.maybe_report_stats();
        Ok(())
    }
}

impl BLSSigVerifier {
    pub fn new(
        root_bank_cache: RootBankCache,
        verified_votes_sender: VerifiedVoteSender,
        message_sender: Sender<BLSMessage>,
    ) -> Self {
        Self {
            root_bank_cache,
            verified_votes_sender,
            message_sender,
            stats: BLSSigVerifierStats::new(),
        }
    }

    fn send_verified_votes(&mut self, verified_votes: HashMap<Pubkey, Vec<Slot>>) {
        let mut stats_updater = StatsUpdater::default();
        for (pubkey, slots) in verified_votes {
            match self.verified_votes_sender.try_send((pubkey, slots)) {
                Ok(()) => {
                    stats_updater.verified_votes_sent += 1;
                }
                Err(e) => {
                    trace!("Failed to send verified vote: {}", e);
                    stats_updater.verified_votes_sent_failed += 1;
                }
            }
        }
        self.stats.update(stats_updater);
    }
}

// Add tests for the BLS signature verifier
#[cfg(test)]
mod tests {
    use {
        super::*,
        bitvec::prelude::*,
        crossbeam_channel::Receiver,
        solana_bls_signatures::Signature,
        solana_hash::Hash,
        solana_perf::packet::{Packet, PinnedPacketBatch},
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                create_genesis_config_with_alpenglow_vote_accounts_no_program,
                ValidatorVoteKeypairs,
            },
        },
        solana_signer::Signer,
        solana_votor_messages::{
            bls_message::{
                BLSMessage, Certificate, CertificateMessage, CertificateType, VoteMessage,
            },
            vote::Vote,
        },
        stats::STATS_INTERVAL_DURATION,
        std::time::{Duration, Instant},
    };

    fn create_keypairs_and_bls_sig_verifier(
        verified_vote_sender: VerifiedVoteSender,
        message_sender: Sender<BLSMessage>,
    ) -> (Vec<ValidatorVoteKeypairs>, BLSSigVerifier) {
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let stakes_vec = (0..validator_keypairs.len())
            .map(|i| 1_000 - i as u64)
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts_no_program(
            1_000_000_000,
            &validator_keypairs,
            stakes_vec,
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank0);
        let root_bank_cache = RootBankCache::new(bank_forks);
        (
            validator_keypairs,
            BLSSigVerifier::new(root_bank_cache, verified_vote_sender, message_sender),
        )
    }

    fn test_bls_message_transmission(
        verifier: &mut BLSSigVerifier,
        receiver: Option<&Receiver<BLSMessage>>,
        messages: &[BLSMessage],
        expect_send_packets_ok: bool,
    ) {
        let packets = messages
            .iter()
            .map(|msg| {
                let mut packet = Packet::default();
                packet
                    .populate_packet(None, msg)
                    .expect("Failed to populate packet");
                packet
            })
            .collect::<Vec<Packet>>();
        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];
        if expect_send_packets_ok {
            assert!(verifier.send_packets(packet_batches).is_ok());
            if let Some(receiver) = receiver {
                for msg in messages {
                    match receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(received_msg) => assert_eq!(received_msg, *msg),
                        Err(e) => warn!("Failed to receive BLS message: {}", e),
                    }
                }
            }
        } else {
            assert!(verifier.send_packets(packet_batches).is_err());
        }
    }

    #[test]
    fn test_blssigverifier_send_packets() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (verified_vote_sender, verfied_vote_receiver) = crossbeam_channel::unbounded();
        // Create bank forks and epoch stakes

        let (validator_keypairs, mut verifier) =
            create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);

        let mut bitmap = BitVec::<u8, Lsb0>::repeat(false, 8);
        bitmap.set(3, true);
        bitmap.set(5, true);
        let vote_rank: usize = 2;

        let certificate = Certificate::new(CertificateType::Finalize, 4, None);

        let messages = vec![
            BLSMessage::Vote(VoteMessage {
                vote: Vote::new_finalization_vote(5),
                signature: Signature::default(),
                rank: vote_rank as u16,
            }),
            BLSMessage::Certificate(CertificateMessage {
                certificate,
                signature: Signature::default(),
                bitmap,
            }),
        ];
        test_bls_message_transmission(&mut verifier, Some(&receiver), &messages, true);
        assert_eq!(verifier.stats.sent, 2);
        assert_eq!(verifier.stats.received, 2);
        assert_eq!(verifier.stats.received_malformed, 0);
        let received_verified_votes = verfied_vote_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes,
            (validator_keypairs[vote_rank].vote_keypair.pubkey(), vec![5])
        );

        let vote_rank: usize = 3;
        let messages = vec![BLSMessage::Vote(VoteMessage {
            vote: Vote::new_notarization_vote(6, Hash::new_unique()),
            signature: Signature::default(),
            rank: vote_rank as u16,
        })];
        test_bls_message_transmission(&mut verifier, Some(&receiver), &messages, true);
        assert_eq!(verifier.stats.sent, 3);
        assert_eq!(verifier.stats.received, 3);
        assert_eq!(verifier.stats.received_malformed, 0);
        let received_verified_votes = verfied_vote_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes,
            (validator_keypairs[vote_rank].vote_keypair.pubkey(), vec![6])
        );

        // Pretend 10 seconds have passed, make sure stats are reset
        verifier.stats.last_stats_logged = Instant::now() - STATS_INTERVAL_DURATION;
        let vote_rank: usize = 9;
        let messages = vec![BLSMessage::Vote(VoteMessage {
            vote: Vote::new_notarization_fallback_vote(7, Hash::new_unique()),
            signature: Signature::default(),
            rank: vote_rank as u16,
        })];
        test_bls_message_transmission(&mut verifier, Some(&receiver), &messages, true);
        // Since we just logged all stats (including the packet just sent), stats should be reset
        assert_eq!(verifier.stats.sent, 0);
        assert_eq!(verifier.stats.received, 0);
        assert_eq!(verifier.stats.received_malformed, 0);
        let received_verified_votes = verfied_vote_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes,
            (validator_keypairs[vote_rank].vote_keypair.pubkey(), vec![7])
        );
    }

    #[test]
    fn test_blssigverifier_send_packets_malformed() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (_, mut verifier) = create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);

        let packets = vec![Packet::default()];
        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];
        assert!(verifier.send_packets(packet_batches).is_ok());
        assert_eq!(verifier.stats.sent, 0);
        assert_eq!(verifier.stats.received, 1);
        assert_eq!(verifier.stats.received_malformed, 1);
        assert_eq!(verifier.stats.received_no_epoch_stakes, 0);

        // Expect no messages since the packet was malformed
        assert!(receiver.is_empty());

        // Send a packet with no epoch stakes
        let messages = vec![BLSMessage::Vote(VoteMessage {
            vote: Vote::new_finalization_vote(5_000_000_000),
            signature: Signature::default(),
            rank: 0,
        })];
        test_bls_message_transmission(&mut verifier, None, &messages, true);
        assert_eq!(verifier.stats.sent, 0);
        assert_eq!(verifier.stats.received, 2);
        assert_eq!(verifier.stats.received_malformed, 1);
        assert_eq!(verifier.stats.received_no_epoch_stakes, 1);

        // Expect no messages since the packet was malformed
        assert!(receiver.is_empty());

        // Send a packet with invalid rank
        let messages = vec![BLSMessage::Vote(VoteMessage {
            vote: Vote::new_finalization_vote(5),
            signature: Signature::default(),
            rank: 1000, // Invalid rank
        })];
        test_bls_message_transmission(&mut verifier, None, &messages, true);
        assert_eq!(verifier.stats.sent, 0);
        assert_eq!(verifier.stats.received, 3);
        assert_eq!(verifier.stats.received_malformed, 2);
        assert_eq!(verifier.stats.received_no_epoch_stakes, 1);

        // Expect no messages since the packet was malformed
        assert!(receiver.is_empty());
    }

    #[test]
    fn test_blssigverifier_send_packets_channel_full() {
        solana_logger::setup();
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (_, mut verifier) = create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);
        let messages = vec![
            BLSMessage::Vote(VoteMessage {
                vote: Vote::new_finalization_vote(5),
                signature: Signature::default(),
                rank: 0,
            }),
            BLSMessage::Vote(VoteMessage {
                vote: Vote::new_notarization_fallback_vote(6, Hash::new_unique()),
                signature: Signature::default(),
                rank: 2,
            }),
        ];
        test_bls_message_transmission(&mut verifier, Some(&receiver), &messages, true);

        // We failed to send the second message because the channel is full.
        assert_eq!(verifier.stats.sent, 1);
        assert_eq!(verifier.stats.received, 2);
        assert_eq!(verifier.stats.received_malformed, 0);
    }

    #[test]
    fn test_blssigverifier_send_packets_receiver_closed() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (_, mut verifier) = create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);
        // Close the receiver, should get panic on next send
        drop(receiver);
        let messages = vec![BLSMessage::Vote(VoteMessage {
            vote: Vote::new_finalization_vote(5),
            signature: Signature::default(),
            rank: 0,
        })];
        test_bls_message_transmission(&mut verifier, None, &messages, false);
    }

    #[test]
    fn test_blssigverifier_send_discarded_packets() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (verified_vote_sender, _) = crossbeam_channel::unbounded();
        let (_, mut verifier) = create_keypairs_and_bls_sig_verifier(verified_vote_sender, sender);
        let message = BLSMessage::Vote(VoteMessage {
            vote: Vote::new_finalization_vote(5),
            signature: Signature::default(),
            rank: 0,
        });
        let mut packet = Packet::default();
        packet
            .populate_packet(None, &message)
            .expect("Failed to populate packet");
        packet.meta_mut().set_discard(true);
        let packets = vec![packet];
        let packet_batches = vec![PinnedPacketBatch::new(packets).into()];
        assert!(verifier.send_packets(packet_batches).is_ok());
        assert_eq!(verifier.stats.sent, 0);
        assert_eq!(verifier.stats.sent_failed, 0);
        assert_eq!(verifier.stats.verified_votes_sent, 0);
        assert_eq!(verifier.stats.verified_votes_sent_failed, 0);
        assert_eq!(verifier.stats.received, 1);
        assert_eq!(verifier.stats.received_discarded, 1);
        assert_eq!(verifier.stats.received_malformed, 0);
        assert_eq!(verifier.stats.received_no_epoch_stakes, 0);
        assert_eq!(verifier.stats.received_votes, 0);
        assert!(receiver.is_empty());
    }
}
