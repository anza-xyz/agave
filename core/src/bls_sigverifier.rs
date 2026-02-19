use {
    crate::cluster_info_vote_listener::VerifiedVoterSlotsSender,
    agave_votor_messages::{
        consensus_message::{Certificate, ConsensusMessage, VoteMessage},
        migration::MigrationStatus,
    },
    crossbeam_channel::{Receiver, Sender, TrySendError},
    solana_clock::Slot,
    solana_perf::packet::PacketBatch,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    solana_streamer::streamer,
    std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, Builder},
    },
};

pub fn spawn_service(
    exit: Arc<AtomicBool>,
    packet_receiver: Receiver<PacketBatch>,
    banks: SharableBanks,
    vote_sender: VerifiedVoterSlotsSender,
    message_sender: Sender<Vec<ConsensusMessage>>,
    migration_status: Arc<MigrationStatus>,
) -> thread::JoinHandle<()> {
    let verifier = BLSSigVerifier::new(banks, vote_sender, message_sender, migration_status);

    Builder::new()
        .name("solSigVerBLS".to_string())
        .spawn(move || verifier.run(exit, packet_receiver))
        .unwrap()
}

pub struct BLSSigVerifier {
    banks: SharableBanks,
    /// Sender to repair service
    vote_sender: VerifiedVoterSlotsSender,
    /// Sender to votor
    message_sender: Sender<Vec<ConsensusMessage>>,
    migration_status: Arc<MigrationStatus>,
}

impl BLSSigVerifier {
    pub fn new(
        banks: SharableBanks,
        vote_sender: VerifiedVoterSlotsSender,
        message_sender: Sender<Vec<ConsensusMessage>>,
        migration_status: Arc<MigrationStatus>,
    ) -> Self {
        Self {
            banks,
            vote_sender,
            message_sender,
            migration_status,
        }
    }

    fn run(mut self, exit: Arc<AtomicBool>, receiver: Receiver<PacketBatch>) {
        info!("BLSSigverifier starting");
        while !exit.load(Ordering::Relaxed) {
            const SOFT_RECEIVE_CAP: usize = 5_000;
            match streamer::recv_packet_batches(&receiver, SOFT_RECEIVE_CAP) {
                Ok((batches, _, _)) => {
                    if self.process_batches(batches).is_err() {
                        break;
                    }
                }
                Err(streamer::StreamerError::RecvTimeout(e)) => {
                    if e.is_disconnected() {
                        break;
                    }
                    continue;
                }
                Err(_) => continue,
            }
        }
        info!("BLSSigverifier shutting down");
    }

    /// Extract votes and certs from the packet batch, verify and send the results
    fn process_batches(&mut self, batches: Vec<PacketBatch>) -> Result<(), ()> {
        if self.migration_status.is_pre_feature_activation() {
            // We only need to start processing messages once the feature flag is active
            return Ok(());
        }

        let root = self.banks.root();
        let (votes, certs) = self.extract_messages(batches, &root);

        let verified_votes = self.verify_votes(votes, &root);
        let verified_certs = self.verify_certificates(certs, &root);

        self.send_results(verified_votes, verified_certs)?;

        Ok(())
    }

    fn extract_messages(
        &self,
        batches: Vec<PacketBatch>,
        _root: &Bank,
    ) -> (Vec<VoteMessage>, Vec<Certificate>) {
        let mut votes = Vec::new();
        let mut certs = Vec::new();

        for batch in batches {
            for packet in batch.into_iter() {
                self.extract_packet(packet, &mut votes, &mut certs);
            }
        }

        (votes, certs)
    }

    /// Retrieve the bls message from the packet, performing any filtering checks as needed
    fn extract_packet(
        &self,
        packet: solana_perf::packet::PacketRef,
        votes: &mut Vec<VoteMessage>,
        certs: &mut Vec<Certificate>,
    ) {
        if packet.meta().discard() {
            return;
        }

        let Ok(msg) = packet.deserialize_slice::<ConsensusMessage, _>(..) else {
            return;
        };

        match msg {
            ConsensusMessage::Vote(vote) => votes.push(vote),
            ConsensusMessage::Certificate(cert) => certs.push(cert),
        }
    }

    fn verify_votes(&self, votes: Vec<VoteMessage>, bank: &Bank) -> Vec<(VoteMessage, Pubkey)> {
        // Actual BLS signature verification to follow
        votes
            .into_iter()
            .filter_map(|vote| {
                let entry = bank
                    .current_epoch_stakes()
                    .bls_pubkey_to_rank_map()
                    .get_pubkey_stake_entry(vote.rank as usize)?;
                Some((vote, entry.pubkey))
            })
            .collect()
    }

    fn verify_certificates(&self, certs: Vec<Certificate>, _bank: &Bank) -> Vec<Certificate> {
        // Actual certificate verification to follow
        certs
    }

    fn send_results(
        &self,
        votes: Vec<(VoteMessage, Pubkey)>,
        certs: Vec<Certificate>,
    ) -> Result<(), ()> {
        let mut votes_by_pubkey: HashMap<Pubkey, Vec<Slot>> = HashMap::new();

        // Send votes
        for (vote, pubkey) in &votes {
            votes_by_pubkey
                .entry(*pubkey)
                .or_default()
                .push(vote.vote.slot());
        }
        let votes = votes
            .into_iter()
            .map(|(v, _)| ConsensusMessage::Vote(v))
            .collect();
        self.send(votes)?;

        // Send certificates
        let certs = certs
            .into_iter()
            .map(ConsensusMessage::Certificate)
            .collect();
        self.send(certs)?;

        // Send votes to repair service
        for (pubkey, slots) in votes_by_pubkey {
            let _ = self.vote_sender.try_send((pubkey, slots));
        }

        Ok(())
    }

    fn send(&self, msgs: Vec<ConsensusMessage>) -> Result<(), ()> {
        match self.message_sender.try_send(msgs) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err(()),
        }
    }
}
