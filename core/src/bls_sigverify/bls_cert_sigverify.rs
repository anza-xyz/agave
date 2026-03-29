use {
    super::{bls_sigverifier::BAN_TIMEOUT, errors::SigVerifyCertError, stats::SigVerifyCertStats},
    crate::bls_sigverify::{bls_sigverifier::NUM_SLOTS_FOR_VERIFY, utils::send_certs_to_pool},
    agave_bls_cert_verify::cert_verify::Error as BlsCertVerifyError,
    agave_votor_messages::{
        consensus_message::{Certificate, CertificateType, ConsensusMessage},
        fraction::Fraction,
    },
    crossbeam_channel::Sender,
    rayon::{
        ThreadPool,
        iter::{IntoParallelIterator, ParallelIterator},
    },
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::{collections::HashSet, num::NonZeroU64},
    thiserror::Error,
};

#[derive(Clone, Debug)]
pub(super) struct CertPayload {
    pub(super) cert: Certificate,
    pub(super) remote_pubkey: Pubkey,
}

#[derive(Debug, Error)]
enum CertVerifyError {
    #[error("Cert Verification Error {0}")]
    CertVerifyFailed(#[from] BlsCertVerifyError),
    #[error("Not enough stake {aggregate_stake}: {cert_fraction} < {required_fraction}")]
    NotEnoughStake {
        aggregate_stake: u64,
        cert_fraction: Fraction,
        required_fraction: Fraction,
    },
    #[error("discarding cert with slot {cert_slot} too far in future from root slot {root_slot}")]
    TooFarInFuture { cert_slot: Slot, root_slot: Slot },
}

/// Verifies certificates and sends the verified certificates to the consensus pool.
///
/// Additionally inserts valid [`CertificateType`]s into `verified_certs_set`.
/// Any certificate that fails verification will have its sender banlisted.
///
/// Function expects that the caller has already deduped the certs to verify i.e.
/// none of the certs appear in the [`verified_certs_set`].
pub(super) fn verify_and_send_certificates(
    verified_certs_set: &mut HashSet<CertificateType>,
    certs: Vec<CertPayload>,
    root_bank: &Bank,
    channel_to_pool: &Sender<Vec<ConsensusMessage>>,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
) -> Result<SigVerifyCertStats, SigVerifyCertError> {
    for cert in certs.iter().map(|cert_payload| &cert_payload.cert) {
        debug_assert!(!verified_certs_set.contains(&cert.cert_type));
    }
    let mut measure = Measure::start("verify_and_send_certificates");
    let mut stats = SigVerifyCertStats::default();

    if certs.is_empty() {
        return Ok(stats);
    }

    stats.certs_to_sig_verify += certs.len() as u64;
    let messages = verify_certs(
        certs,
        root_bank,
        verified_certs_set,
        &mut stats,
        banlist,
        thread_pool,
    );
    stats.sig_verified_certs += messages.len() as u64;
    send_certs_to_pool(messages, channel_to_pool, &mut stats)?;

    measure.stop();
    stats
        .fn_verify_and_send_certs_stats
        .add_sample(measure.as_us());
    Ok(stats)
}

/// Verifies certificates in `certs`, stores a local copy, and prepares them for forwarding.
///
/// The valid certs are inserted into the [`verified_certs_set`].
/// Invalid cert senders are banlisted.
/// Returns a Vec of [`ConsensusMessage`] constructed from the valid certs.
fn verify_certs(
    certs: Vec<CertPayload>,
    root_bank: &Bank,
    verified_certs_set: &mut HashSet<CertificateType>,
    stats: &mut SigVerifyCertStats,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
) -> Vec<ConsensusMessage> {
    let verified = thread_pool.install(|| {
        certs
            .into_par_iter()
            .map(|cert_payload| {
                let res = verify_cert(&cert_payload.cert, root_bank);
                (cert_payload, res)
            })
            .collect::<Vec<_>>()
    });

    verified
        .into_iter()
        .filter_map(|(cert_payload, res)| match res {
            Ok(()) => {
                let cert = cert_payload.cert;
                if !verified_certs_set.insert(cert.cert_type) {
                    stats.unnecessary_certs_verified += 1;
                }
                Some(ConsensusMessage::Certificate(cert))
            }
            Err(e) => {
                banlist.ban(cert_payload.remote_pubkey, BAN_TIMEOUT);

                match e {
                    CertVerifyError::NotEnoughStake { .. } => {
                        stats.stake_verification_failed += 1;
                    }
                    CertVerifyError::CertVerifyFailed(_) => {
                        stats.signature_verification_failed += 1;
                    }
                    CertVerifyError::TooFarInFuture { .. } => {
                        stats.too_far_in_future += 1;
                    }
                };
                None
            }
        })
        .collect()
}

fn verify_cert(cert: &Certificate, root_bank: &Bank) -> Result<(), CertVerifyError> {
    let cert_slot = cert.cert_type.slot();
    let root_slot = root_bank.slot();
    if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
        return Err(CertVerifyError::TooFarInFuture {
            cert_slot,
            root_slot,
        });
    }
    let (aggregate_stake, total_stake) = root_bank.verify_certificate(cert)?;
    debug_assert!(aggregate_stake <= total_stake);
    verify_stake(cert, aggregate_stake, total_stake)
}

fn verify_stake(
    cert: &Certificate,
    aggregate_stake: u64,
    total_stake: u64,
) -> Result<(), CertVerifyError> {
    let (required_fraction, _) = cert.cert_type.limits_and_vote_types();
    let total_stake = NonZeroU64::new(total_stake).expect("Total stake cannot be zero");
    let cert_fraction = Fraction::new(aggregate_stake, total_stake);
    if cert_fraction >= required_fraction {
        Ok(())
    } else {
        Err(CertVerifyError::NotEnoughStake {
            aggregate_stake,
            cert_fraction,
            required_fraction,
        })
    }
}
