use {
    crate::{
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY, SigVerifierChannels},
        errors::SigVerifyCertError,
        stats::SigVerifyCertStats,
        utils::{send_certs_to_pool, send_certs_to_rewards},
    },
    agave_bls_cert_verify::cert_verify::Error as BlsCertVerifyError,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        unverified_vote_message::UnverifiedCertificate,
    },
    agave_votor_transport::endpoint::BanSender,
    log::info,
    rayon::{
        ThreadPool,
        iter::{IntoParallelIterator, ParallelIterator},
    },
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::collections::{HashMap, HashSet},
};

pub(super) struct CertPayload {
    pub(super) cert: UnverifiedCertificate,
    pub(super) sender_identity_pubkey: Pubkey,
    /// Whether rewards need the certificate, see
    /// [`rewards_wants_cert`](crate::rewards::rewards_wants_cert).
    pub(super) rewards_wants_cert: bool,
}

/// Certificates to verify, grouped by [`CertificateType`].
#[derive(Default)]
pub(super) struct CertGroups {
    /// Types that consensus still needs.
    pub(super) consensus: HashMap<CertificateType, Vec<CertPayload>>,
    /// Types that only rewards still need.
    pub(super) rewards: HashMap<CertificateType, Vec<CertPayload>>,
}

struct CertVerifyOutcome {
    verified_cert: Option<Certificate>,
    /// Whether rewards need `verified_cert`.
    rewards_wants_cert: bool,
    failures: Vec<(BlsCertVerifyError, Pubkey)>,
}

/// Verifies certificates and sends the verified certificates to the consensus pool, and the ones
/// rewards need to rewards.
///
/// Additionally inserts valid [`CertificateType`]s into `verified_certs_set`, or into
/// `certs_verified_for_rewards` for the types that only rewards need.
/// Any certificate that fails verification will have its sender banlisted.
///
/// Function expects that the caller has already filtered any certs that appear in the
/// [`verified_certs_set`] or [`certs_verified_for_rewards`] and grouped certs with the same
/// [`CertificateType`]. Each group is verified until the first valid candidate is found.
pub(super) fn verify_and_send_certificates(
    my_pubkey: &Pubkey,
    verified_certs_set: &mut HashSet<CertificateType>,
    certs_verified_for_rewards: &mut HashSet<CertificateType>,
    cert_groups: CertGroups,
    root_bank: &Bank,
    channels: &SigVerifierChannels,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> Result<SigVerifyCertStats, SigVerifyCertError> {
    let CertGroups { consensus, rewards } = cert_groups;
    for cert_type in consensus.keys() {
        debug_assert!(cert_type.slot() <= root_bank.slot().saturating_add(NUM_SLOTS_FOR_VERIFY));
        debug_assert!(!verified_certs_set.contains(cert_type));
    }
    for cert_type in rewards.keys() {
        debug_assert!(!certs_verified_for_rewards.contains(cert_type));
    }
    let mut measure = Measure::start("verify_and_send_certificates");
    let mut stats = SigVerifyCertStats::default();

    if consensus.is_empty() && rewards.is_empty() {
        return Ok(stats);
    }

    let mut certs = Vec::new();
    let mut certs_for_rewards = Vec::new();
    for (cert, rewards_wants_cert) in verify_cert_groups(
        consensus,
        root_bank,
        verified_certs_set,
        &mut stats,
        ban_sender,
        thread_pool,
    ) {
        if rewards_wants_cert {
            certs_for_rewards.push(cert.clone());
        }
        certs.push(cert);
    }
    stats.sig_verified_certs += certs.len() as u64;
    send_certs_to_pool(
        my_pubkey,
        certs,
        &channels.channel_to_pool,
        &mut stats.pool_sender,
    )?;

    // Certificates that only rewards need are verified the same way, after the pool send.
    let reward_only_certs = verify_cert_groups(
        rewards,
        root_bank,
        certs_verified_for_rewards,
        &mut stats,
        ban_sender,
        thread_pool,
    );
    stats.certs_for_rewards_sig_verified += reward_only_certs.len() as u64;
    certs_for_rewards.extend(reward_only_certs.into_iter().map(|(cert, _)| cert));
    send_certs_to_rewards(
        my_pubkey,
        certs_for_rewards,
        &channels.channel_to_reward,
        &mut stats.rewards_sender,
    );

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
/// Returns a list of [`Certificate`]s constructed from the valid certs, with whether rewards need
/// each of them.
fn verify_cert_groups(
    cert_groups: HashMap<CertificateType, Vec<CertPayload>>,
    root_bank: &Bank,
    verified_certs_set: &mut HashSet<CertificateType>,
    stats: &mut SigVerifyCertStats,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> Vec<(Certificate, bool)> {
    let results = thread_pool.install(|| {
        cert_groups
            .into_par_iter()
            .map(|(_, certs)| {
                let num_certs = certs.len();
                (num_certs, verify_cert_group(certs, root_bank, thread_pool))
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
                certs.push((cert, outcome.rewards_wants_cert));
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

    for cert_payload in certs {
        match verify_cert(cert_payload.cert, root_bank, thread_pool) {
            Ok(cert) => {
                return CertVerifyOutcome {
                    verified_cert: Some(cert),
                    rewards_wants_cert: cert_payload.rewards_wants_cert,
                    failures,
                };
            }
            Err(err) => failures.push((err, cert_payload.sender_identity_pubkey)),
        }
    }

    CertVerifyOutcome {
        verified_cert: None,
        rewards_wants_cert: false,
        failures,
    }
}

fn verify_cert(
    cert: UnverifiedCertificate,
    root_bank: &Bank,
    thread_pool: &ThreadPool,
) -> Result<Certificate, BlsCertVerifyError> {
    let cert_slot = cert.cert_type.slot();
    let root_slot = root_bank.slot();
    debug_assert!(cert_slot <= root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY));
    root_bank.verify_certificate(cert, Some(thread_pool))
}
