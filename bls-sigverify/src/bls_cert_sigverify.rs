use {
    crate::{
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
    crossbeam_channel::Sender,
    log::info,
    rayon::{
        ThreadPool,
        iter::{IntoParallelIterator, ParallelIterator},
    },
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::{
        collections::{HashMap, HashSet},
        time::Instant,
    },
    thiserror::Error,
};

pub(super) struct CertPayload {
    pub(super) cert: UnverifiedCertificate,
    pub(super) sender_identity_pubkey: Pubkey,
}

#[derive(Debug, Error)]
enum CertVerifyError {
    #[error("Cert Verification Error {0}")]
    CertVerifyFailed(#[from] BlsCertVerifyError),
    #[error("discarding cert with slot {cert_slot} too far in future from root slot {root_slot}")]
    TooFarInFuture { cert_slot: Slot, root_slot: Slot },
}

struct CertVerifyOutcome {
    verified_cert: Option<Certificate>,
    local_stats: SigVerifyCertStats,
    num_attempted: usize,
    newly_banned: Vec<Pubkey>,
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
    verified_certs_set: &mut HashSet<CertificateType>,
    cert_groups: HashMap<CertificateType, Vec<CertPayload>>,
    root_bank: &Bank,
    channel_to_pool: &Sender<SigVerifiedBatch>,
    banlist: &SimpleQosBanlist,
    locally_banned: &HashMap<Pubkey, Instant>,
    thread_pool: &ThreadPool,
) -> Result<(SigVerifyCertStats, Vec<Pubkey>), SigVerifyCertError> {
    for cert_type in cert_groups.keys() {
        debug_assert!(!verified_certs_set.contains(cert_type));
    }
    let mut measure = Measure::start("verify_and_send_certificates");
    let mut stats = SigVerifyCertStats::default();

    if cert_groups.is_empty() {
        return Ok((stats, vec![]));
    }

    let (messages, newly_banned) = verify_certs(
        cert_groups,
        root_bank,
        verified_certs_set,
        &mut stats,
        banlist,
        locally_banned,
        thread_pool,
    );
    stats.sig_verified_certs += messages.len() as u64;
    send_certs_to_pool(messages, channel_to_pool, &mut stats)?;

    measure.stop();
    stats
        .fn_verify_and_send_certs_stats
        .add_sample(measure.as_us());
    Ok((stats, newly_banned))
}

/// Verifies certificates in `cert_groups`, stores a local copy, and prepares them for forwarding.
///
/// The valid certs are inserted into the [`verified_certs_set`].
/// Invalid cert senders are banlisted.
/// Returns a [`SigVerifiedBatch`] constructed from the valid certs.
fn verify_certs(
    cert_groups: HashMap<CertificateType, Vec<CertPayload>>,
    root_bank: &Bank,
    verified_certs_set: &mut HashSet<CertificateType>,
    stats: &mut SigVerifyCertStats,
    banlist: &SimpleQosBanlist,
    locally_banned: &HashMap<Pubkey, Instant>,
    thread_pool: &ThreadPool,
) -> (SigVerifiedBatch, Vec<Pubkey>) {
    let verified = thread_pool.install(|| {
        cert_groups
            .into_par_iter()
            .map(|(_, certs)| {
                let num_certs = certs.len();
                (num_certs, verify_cert_group(certs, root_bank, locally_banned, banlist))
            })
            .collect::<Vec<_>>()
    });

    let mut certs = Vec::new();
    let mut newly_banned = Vec::new();
    for (num_certs, outcome) in verified {
        stats.certs_to_sig_verify += outcome.num_attempted as u64;
        stats.redundant_certs_skipped +=
            num_certs.saturating_sub(outcome.num_attempted) as u64;
        stats.merge(outcome.local_stats);
        newly_banned.extend(outcome.newly_banned);

        if let Some(cert) = outcome.verified_cert {
            if verified_certs_set.insert(cert.cert_type) {
                certs.push(cert);
            } else {
                stats.unnecessary_certs_verified += 1;
            }
        }
    }

    (SigVerifiedBatch::Certificates(certs), newly_banned)
}

fn verify_cert_group(
    certs: Vec<CertPayload>,
    root_bank: &Bank,
    locally_banned: &HashMap<Pubkey, Instant>,
    banlist: &SimpleQosBanlist,
) -> CertVerifyOutcome {
    let mut local_stats = SigVerifyCertStats::default();
    let mut num_attempted = 0;
    let mut newly_banned = Vec::new();
    // Tracks senders that failed earlier in this same group so we skip their duplicates.
    let mut intra_group_banned = HashSet::new();

    for cert_payload in certs {
        if locally_banned.contains_key(&cert_payload.sender_identity_pubkey)
            || intra_group_banned.contains(&cert_payload.sender_identity_pubkey)
        {
            continue;
        }
        num_attempted += 1;
        match verify_cert(cert_payload.cert, root_bank) {
            Ok(cert) => {
                return CertVerifyOutcome {
                    verified_cert: Some(cert),
                    local_stats,
                    num_attempted,
                    newly_banned,
                };
            }
            Err(err) => {
                intra_group_banned.insert(cert_payload.sender_identity_pubkey);
                newly_banned.push(cert_payload.sender_identity_pubkey);
                handle_cert_verify_error(
                    err,
                    cert_payload.sender_identity_pubkey,
                    &mut local_stats,
                    banlist,
                );
            }
        }
    }

    CertVerifyOutcome {
        verified_cert: None,
        local_stats,
        num_attempted,
        newly_banned,
    }
}

fn handle_cert_verify_error(
    err: CertVerifyError,
    sender_identity_pubkey: Pubkey,
    stats: &mut SigVerifyCertStats,
    banlist: &SimpleQosBanlist,
) {
    match &err {
        CertVerifyError::CertVerifyFailed(_) => {
            stats.banning_validator += 1;
            if banlist.ban(sender_identity_pubkey, BAN_TIMEOUT) {
                stats.already_banned += 1;
            } else {
                info!(
                    "bls_cert_sigverify: banned sender={sender_identity_pubkey} due to error {err}"
                );
            }
            stats.certificate_verification_failed += 1;
        }
        CertVerifyError::TooFarInFuture { .. } => {
            stats.too_far_in_future += 1;
        }
    }
}

fn verify_cert(
    cert: UnverifiedCertificate,
    root_bank: &Bank,
) -> Result<Certificate, CertVerifyError> {
    let cert_slot = cert.cert_type.slot();
    let root_slot = root_bank.slot();
    if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
        return Err(CertVerifyError::TooFarInFuture {
            cert_slot,
            root_slot,
        });
    }
    let cert = root_bank.verify_certificate(cert)?;
    Ok(cert)
}
