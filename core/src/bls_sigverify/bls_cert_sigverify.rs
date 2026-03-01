use {
    super::{errors::SigVerifyCertError, stats::SigVerifyCertStats},
    crate::bls_sigverify::utils::send_certs_to_pool,
    agave_bls_cert_verify::cert_verify::Error as BlsCertVerifyError,
    agave_votor_messages::{
        consensus_message::{Certificate, CertificateType, ConsensusMessage},
        fraction::Fraction,
    },
    crossbeam_channel::{Sender, bounded},
    rayon::iter::{IntoParallelIterator, ParallelIterator},
    solana_measure::measure::Measure,
    solana_runtime::bank::Bank,
    std::{collections::HashSet, num::NonZeroU64},
    thiserror::Error,
};

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
}

/// Verifies certs and sends the verified certs to the consensus pool.
///
/// Additionally inserts valid [`CertificateType`]s into [`verified_certs_sets`].
///
/// Function expects that the caller has already deduped the certs to verify i.e.
/// none of the certs appear in the [`verified_certs_set`].
pub(super) fn verify_and_send_certificates(
    verified_certs_set: &mut HashSet<CertificateType>,
    certs: Vec<Certificate>,
    bank: &Bank,
    channel_to_pool: &Sender<Vec<ConsensusMessage>>,
) -> Result<SigVerifyCertStats, SigVerifyCertError> {
    for cert in certs.iter() {
        debug_assert!(!verified_certs_set.contains(&cert.cert_type));
    }
    let mut measure = Measure::start("verify_and_send_certificates");
    let mut stats = SigVerifyCertStats::default();

    if certs.is_empty() {
        return Ok(stats);
    }

    stats.certs_to_sig_verify += certs.len() as u64;
    let messages = verify_certs(verified_certs_set, certs, bank, &mut stats);
    stats.sig_verified_certs += messages.len() as u64;
    send_certs_to_pool(messages, channel_to_pool, &mut stats)?;

    measure.stop();
    stats
        .fn_verify_and_send_certs_stats
        .increment(measure.as_us())
        .unwrap();
    Ok(stats)
}

/// Verifies certs.
///
/// The valid certs are inserted into the [`verified_certs_set`].
/// Returns a Vec of [`ConsensusMessage`] constructed from the valid certs.
fn verify_certs(
    verified_certs_set: &mut HashSet<CertificateType>,
    certs: Vec<Certificate>,
    bank: &Bank,
    stats: &mut SigVerifyCertStats,
) -> Vec<ConsensusMessage> {
    // We want to verify the certs in parallel however collecting them and inserting them into the
    // set has to happen sequentially.  Following allows us to do that while minimising the number
    // of times we have to iterate over the list of certs.

    let (tx, rx) = bounded(certs.len());
    // Assuming that sigverifier's dedicated thread pool was used to call this function, the
    // following should run on that thread pool.
    rayon::scope(|s| {
        s.spawn(|_| {
            certs.into_par_iter().for_each_with(tx, |tx, cert| {
                tx.send((verify_cert(&cert, bank), cert)).unwrap()
            })
        })
    });
    rx.into_iter()
        .filter_map(|(res, cert)| match res {
            Ok(()) => {
                verified_certs_set.insert(cert.cert_type);
                Some(ConsensusMessage::Certificate(cert))
            }
            Err(e) => match e {
                CertVerifyError::NotEnoughStake { .. } => {
                    stats.stake_verification_failed += 1;
                    None
                }
                CertVerifyError::CertVerifyFailed(e) => match e {
                    BlsCertVerifyError::MissingRankMap => None,
                    _ => {
                        stats.signature_verification_failed += 1;
                        None
                    }
                },
            },
        })
        .collect()
}

fn verify_cert(cert: &Certificate, bank: &Bank) -> Result<(), CertVerifyError> {
    let (aggregate_stake, total_stake) = bank.verify_certificate(cert)?;
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
