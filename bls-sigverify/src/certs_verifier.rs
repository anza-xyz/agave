use {
    crate::{
        bls_cert_sigverify::CertPayload,
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY},
        errors::SigVerifyCertError,
        stats::SigVerifyCertStats,
        utils::send_certs_to_pool,
    },
    agave_bls_cert_verify::cert_verify::Error as BlsCertVerifyError,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        sig_verified_messages::SigVerifiedBatch,
        unverified_vote_message::UnverifiedCertificate,
    },
    agave_votor_transport::endpoint::BanSender,
    crossbeam_channel::{Receiver, Sender},
    log::{error, info},
    rayon::{
        ThreadPool,
        iter::{IntoParallelIterator, ParallelIterator},
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    },
};

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
        }
    }

    fn recv(&self) -> Result<HashMap<CertificateType, Vec<CertPayload>>, ()> {
        self.unverified_certs_receiver.recv().map_err(|_| ())
    }

    fn maybe_prune(&mut self, root_bank: &Bank) {
        let root_slot = root_bank.slot();
        if self.last_checked_root_slot < root_slot {
            self.last_checked_root_slot = root_slot;
            self.verified_certs.retain(|cert| cert.slot() >= root_slot);
        }
    }

    pub(crate) fn run(mut self) {
        while !self.exit.load(Ordering::Relaxed) {
            let Ok(cert_groups) = self.recv() else {
                error!("unverified certs receiver channel disconnected.  Exiting.");
                break;
            };
            let my_pubkey = self.cluster_info.id();
            let root_bank = self.sharable_banks.root();
            match verify_and_send_certificates(
                &my_pubkey,
                &mut self.verified_certs,
                cert_groups,
                &root_bank,
                &self.channel_to_pool,
                &self.ban_sender,
                &self.thread_pool,
            ) {
                Ok(_) => {}
                Err(e) => {
                    error!("verified certs failed with {e:?}.  Exiting.");
                    break;
                }
            }
            self.maybe_prune(&root_bank);
        }
    }
}

/// Verifies certificates and sends the verified certificates to the consensus pool.
///
/// Additionally inserts valid [`CertificateType`]s into `verified_certs_set`.
/// Any certificate that fails verification will have its sender banlisted.
///
/// Function expects that the caller has already filtered any certs that appear in the
/// [`verified_certs_set`] and grouped certs with the same [`CertificateType`]. Each group is
/// verified until the first valid candidate is found.
pub(super) fn verify_and_send_certificates(
    my_pubkey: &Pubkey,
    verified_certs_set: &mut HashSet<CertificateType>,
    mut cert_groups: HashMap<CertificateType, Vec<CertPayload>>,
    root_bank: &Bank,
    channel_to_pool: &Sender<SigVerifiedBatch>,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> Result<SigVerifyCertStats, SigVerifyCertError> {
    for cert_type in cert_groups.keys() {
        debug_assert!(cert_type.slot() <= root_bank.slot().saturating_add(NUM_SLOTS_FOR_VERIFY));
    }
    let _num_duplicate_certs = remove_verified_cert_groups(verified_certs_set, &mut cert_groups);
    let mut measure = Measure::start("verify_and_send_certificates");
    let mut stats = SigVerifyCertStats::default();

    if cert_groups.is_empty() {
        return Ok(stats);
    }

    let messages = verify_cert_groups(
        cert_groups,
        root_bank,
        verified_certs_set,
        &mut stats,
        ban_sender,
        thread_pool,
    );
    stats.sig_verified_certs += messages.len() as u64;
    send_certs_to_pool(my_pubkey, messages, channel_to_pool, &mut stats.pool_sender)?;

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
/// Returns a [`SigVerifiedBatch`] constructed from the valid certs.
fn verify_cert_groups(
    cert_groups: HashMap<CertificateType, Vec<CertPayload>>,
    root_bank: &Bank,
    verified_certs_set: &mut HashSet<CertificateType>,
    stats: &mut SigVerifyCertStats,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> SigVerifiedBatch {
    let results = thread_pool.install(|| {
        cert_groups
            .into_par_iter()
            .map(|(_, certs)| {
                let num_certs = certs.len();
                (num_certs, verify_cert_group(certs, root_bank))
            })
            .collect::<Vec<_>>()
    });

    let mut certs = Vec::new();
    for (num_certs, outcome) in results {
        let num_certs_attempted = if outcome.verified_cert.is_some() {
            outcome.failures.len().saturating_add(1)
        } else {
            outcome.failures.len()
        };
        stats.certs_to_sig_verify += num_certs_attempted as u64;
        stats.redundant_certs_skipped += num_certs.saturating_sub(num_certs_attempted) as u64;

        for (err, sender_identity_pubkey) in outcome.failures {
            stats.banning_validator += 1;
            ban_sender.ban(sender_identity_pubkey, BAN_TIMEOUT);
            info!("bls_cert_sigverify: banned sender={sender_identity_pubkey} due to error {err}");
            stats.certificate_verification_failed += 1;
        }

        if let Some(cert) = outcome.verified_cert {
            if verified_certs_set.insert(cert.cert_type) {
                certs.push(cert);
            } else {
                stats.unnecessary_certs_verified += 1;
            }
        }
    }

    SigVerifiedBatch::Certificates(certs)
}

fn verify_cert_group(certs: Vec<CertPayload>, root_bank: &Bank) -> CertVerifyOutcome {
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

fn remove_verified_cert_groups(
    verified_certs: &HashSet<CertificateType>,
    cert_groups: &mut HashMap<CertificateType, Vec<CertPayload>>,
) -> usize {
    let mut duplicates = 0usize;

    cert_groups.retain(|cert_type, certs| {
        if verified_certs.contains(cert_type) {
            duplicates = duplicates.saturating_add(certs.len());
            false
        } else {
            true
        }
    });
    duplicates
}
