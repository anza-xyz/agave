use {
    crate::{
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY},
        stats::CertsVerifierStats,
        utils::send_certs_to_pool,
    },
    agave_bls_cert_verify::cert_verify::Error as BlsCertVerifyError,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        sig_verified_messages::SigVerifiedBatch,
        unverified_vote_message::UnverifiedCertificate,
    },
    agave_votor_transport::endpoint::BanSender,
    crossbeam_channel::{Receiver, Sender, select},
    log::{error, info},
    rayon::{
        ThreadPool,
        iter::{IntoParallelIterator, ParallelIterator},
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure_us,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    },
};

pub(crate) struct CertPayload {
    pub(crate) cert: UnverifiedCertificate,
    pub(crate) sender_identity_pubkey: Pubkey,
}

struct CertVerifyOutcome {
    verified_cert: Option<Certificate>,
    failures: Vec<(BlsCertVerifyError, Pubkey)>,
}

pub(crate) struct CertsVerifier {
    exit: Arc<AtomicBool>,
    cluster_info: Arc<ClusterInfo>,
    verified_certs: HashSet<CertificateType>,
    last_checked_root_slot: Slot,
    sharable_banks: SharableBanks,
    ban_sender: BanSender,
    thread_pool: Arc<ThreadPool>,
    unverified_certs_receiver: Receiver<HashMap<CertificateType, Vec<CertPayload>>>,
    channel_to_pool: Sender<SigVerifiedBatch>,
    stats: CertsVerifierStats,
}

impl CertsVerifier {
    pub(crate) fn new(
        exit: Arc<AtomicBool>,
        unverified_certs_receiver: Receiver<HashMap<CertificateType, Vec<CertPayload>>>,
        cluster_info: Arc<ClusterInfo>,
        sharable_banks: SharableBanks,
        channel_to_pool: Sender<SigVerifiedBatch>,
        ban_sender: BanSender,
        thread_pool: Arc<ThreadPool>,
    ) -> Self {
        Self {
            exit,
            unverified_certs_receiver,
            cluster_info,
            sharable_banks,
            channel_to_pool,
            ban_sender,
            thread_pool,
            verified_certs: HashSet::new(),
            last_checked_root_slot: 0,
            stats: CertsVerifierStats::default(),
        }
    }

    pub(crate) fn run(mut self) {
        while !self.exit.load(Ordering::Relaxed) {
            let Ok(cert_groups) = self.recv() else {
                error!("unverified certs receiver channel disconnected.  Exiting.");
                break;
            };
            let root_bank = self.sharable_banks.root();
            let (verified_certs, verify_certs_us) =
                measure_us!(self.verify_cert_groups(&root_bank, cert_groups));
            let num_certs_verified = verified_certs.len();
            let my_pubkey = self.cluster_info.id();
            if let Err(e) = send_certs_to_pool(
                &my_pubkey,
                verified_certs,
                &self.channel_to_pool,
                &mut self.stats,
            ) {
                error!("sending verified certs failed with {e:?}.  Exiting.");
                break;
            }
            self.maybe_prune(&root_bank);
            self.stats.iterations += 1;
            self.stats.verify_certs_us.add_sample(verify_certs_us);
            self.stats.certs_verified += num_certs_verified;
            self.stats.maybe_report();
        }
    }

    fn recv(&self) -> Result<HashMap<CertificateType, Vec<CertPayload>>, ()> {
        while !self.exit.load(Ordering::Relaxed) {
            select! {
                recv(self.unverified_certs_receiver) -> msg => {
                    return msg.map_err(|_| ())
                }
                default(Duration::from_secs(1)) => continue,
            }
        }
        Err(())
    }

    fn maybe_prune(&mut self, root_bank: &Bank) {
        let root_slot = root_bank.slot();
        if self.last_checked_root_slot < root_slot {
            self.last_checked_root_slot = root_slot;
            self.verified_certs.retain(|cert| cert.slot() >= root_slot);
        }
    }

    fn verify_cert_groups(
        &mut self,
        root_bank: &Bank,
        cert_groups: HashMap<CertificateType, Vec<CertPayload>>,
    ) -> Vec<Certificate> {
        for cert_type in cert_groups.keys() {
            debug_assert!(
                cert_type.slot() <= root_bank.slot().saturating_add(NUM_SLOTS_FOR_VERIFY)
            );
        }
        let results = self.thread_pool.install(|| {
            cert_groups
                .into_par_iter()
                .filter_map(|(cert_type, certs)| {
                    (!self.verified_certs.contains(&cert_type))
                        .then(|| verify_cert_group(certs, root_bank))
                })
                .collect::<Vec<_>>()
        });

        let mut verified_certs = Vec::new();
        for outcome in results {
            self.stats.validators_banned += outcome.failures.len();
            for (err, sender_identity_pubkey) in outcome.failures {
                self.ban_sender.ban(sender_identity_pubkey, BAN_TIMEOUT);
                info!(
                    "bls_cert_sigverify: banned sender={sender_identity_pubkey} due to error {err}"
                );
            }

            if let Some(cert) = outcome.verified_cert
                && self.verified_certs.insert(cert.cert_type)
            {
                verified_certs.push(cert);
            }
        }
        verified_certs
    }
}

fn verify_cert_group(certs: Vec<CertPayload>, root_bank: &Bank) -> CertVerifyOutcome {
    // All the certs should be of the same `CertificateType`
    #[cfg(debug_assertions)]
    {
        let cert_types = certs
            .iter()
            .map(|c| c.cert.cert_type)
            .collect::<HashSet<_>>();
        assert_eq!(cert_types.len(), 1);
    }
    let mut failures = Vec::new();
    for cert_payload in certs {
        match verify_cert(cert_payload.cert, root_bank) {
            Ok(cert) => {
                return CertVerifyOutcome {
                    verified_cert: Some(cert),
                    failures,
                };
            }
            Err(err) => failures.push((err, cert_payload.sender_identity_pubkey)),
        }
    }

    CertVerifyOutcome {
        verified_cert: None,
        failures,
    }
}

fn verify_cert(
    cert: UnverifiedCertificate,
    root_bank: &Bank,
) -> Result<Certificate, BlsCertVerifyError> {
    let cert_slot = cert.cert_type.slot();
    let root_slot = root_bank.slot();
    debug_assert!(cert_slot <= root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY));
    root_bank.verify_certificate(cert)
}

#[cfg(test)]
mod metrics_tests {
    use {
        super::*, crate::test_utils::MetricsTestContext,
        agave_votor_transport::endpoint::stub_ban_channel_for_tests, crossbeam_channel::unbounded,
        rayon::ThreadPoolBuilder,
    };

    #[test]
    fn metrics_count_failed_candidates_but_not_skipped_certificates() {
        let ctx = MetricsTestContext::new();
        let (ban_sender, mut bans) = stub_ban_channel_for_tests(16);
        let mut verifier = CertsVerifier::new(
            Arc::new(AtomicBool::new(false)),
            unbounded().1,
            ctx.cluster_info.clone(),
            ctx.banks.clone(),
            unbounded().0,
            ban_sender,
            Arc::new(ThreadPoolBuilder::new().num_threads(4).build().unwrap()),
        );
        let valid = ctx.certificate(CertificateType::Finalize(1));
        let mut invalid = valid.clone();
        invalid.signature = ctx.validators[0].bls_keypair.sign(b"wrong payload").into();
        let peer = Pubkey::new_unique();
        let payload = |cert| CertPayload {
            cert,
            sender_identity_pubkey: peer,
        };
        let mut groups = HashMap::new();
        groups.insert(
            valid.cert_type,
            vec![
                payload(invalid.clone()),
                payload(valid.clone()),
                payload(invalid.clone()),
            ],
        );
        let mut all_invalid = invalid.clone();
        all_invalid.cert_type = CertificateType::Finalize(2);
        groups.insert(all_invalid.cert_type, vec![payload(all_invalid)]);
        let certs = verifier.verify_cert_groups(&ctx.banks.root(), groups);
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0].cert_type, valid.cert_type);
        assert_eq!(verifier.stats.validators_banned.0, 2);
        assert!(bans.try_recv().is_ok());
        assert!(bans.try_recv().is_ok());
        assert!(bans.try_recv().is_err());

        let certs = verifier.verify_cert_groups(
            &ctx.banks.root(),
            HashMap::from([(valid.cert_type, vec![payload(invalid)])]),
        );
        assert!(certs.is_empty());
        assert_eq!(verifier.stats.validators_banned.0, 2);
        assert!(bans.try_recv().is_err());
    }
}
