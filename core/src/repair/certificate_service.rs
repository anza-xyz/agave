//! The `certificate_service` handles critical certificate related activites:
//! - Storage of critical certificates in blockstore
//! - TODO: Broadcast of new critical certificates to the cluster
//! - TODO: Repair of missing critical certificates to enable progress

use {
    crate::result::{Error, Result},
    crossbeam_channel::{Receiver, RecvTimeoutError},
    solana_ledger::blockstore::Blockstore,
    solana_votor_messages::bls_message::{Certificate, CertificateMessage},
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub(crate) type CertificateReceiver = Receiver<(Certificate, CertificateMessage)>;
pub struct CertificateService {
    t_cert_insert: JoinHandle<()>,
}

impl CertificateService {
    pub(crate) fn new(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        certificate_receiver: CertificateReceiver,
    ) -> Self {
        let t_cert_insert =
            Self::start_certificate_insert_broadcast(exit, blockstore, certificate_receiver);
        Self { t_cert_insert }
    }

    fn start_certificate_insert_broadcast(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        certificate_receiver: CertificateReceiver,
    ) -> JoinHandle<()> {
        let handle_error = || {
            inc_new_counter_error!("solana-certificate-service-error", 1, 1);
        };

        Builder::new()
            .name("solCertInsertBCast".to_string())
            .spawn(move || {
                while !exit.load(Ordering::Relaxed) {
                    let certs = match Self::receive_new_certificates(&certificate_receiver) {
                        Ok(certs) => certs,
                        Err(e) if Self::should_exit_on_error(&e, &handle_error) => break,
                        Err(_e) => continue,
                    };

                    // TODO: broadcast to gossip / all-2-all

                    // TODO: update highest cert local-state and potentially ask for repair for missing certificates
                    // e,g, our previous highest cert was 5, we now see certs for 7 & 8, notify repair to get the cert for 6

                    // Insert into blockstore
                    if let Err(e) = certs.into_iter().try_for_each(|(cert_id, cert)| {
                        Self::insert_certificate(blockstore.as_ref(), cert_id, cert)
                    }) {
                        if Self::should_exit_on_error(&e, &handle_error) {
                            break;
                        }
                    }
                }
            })
            .unwrap()
    }

    fn receive_new_certificates(
        certificate_receiver: &Receiver<(Certificate, CertificateMessage)>,
    ) -> Result<Vec<(Certificate, CertificateMessage)>> {
        const RECV_TIMEOUT: Duration = Duration::from_millis(200);
        Ok(
            std::iter::once(certificate_receiver.recv_timeout(RECV_TIMEOUT)?)
                .chain(certificate_receiver.try_iter())
                .collect(),
        )
    }

    fn insert_certificate(
        blockstore: &Blockstore,
        cert_id: Certificate,
        vote_certificate: CertificateMessage,
    ) -> Result<()> {
        match cert_id {
            Certificate::NotarizeFallback(slot, block_id) => blockstore
                .insert_new_notarization_fallback_certificate(slot, block_id, vote_certificate)?,
            Certificate::Skip(slot) => {
                blockstore.insert_new_skip_certificate(slot, vote_certificate)?
            }
            Certificate::Finalize(_)
            | Certificate::FinalizeFast(_, _)
            | Certificate::Notarize(_, _) => {
                panic!("Programmer error, certificate pool should not notify for {cert_id:?}")
            }
        }
        Ok(())
    }

    fn should_exit_on_error<H>(e: &Error, handle_error: &H) -> bool
    where
        H: Fn(),
    {
        match e {
            Error::RecvTimeout(RecvTimeoutError::Disconnected) => true,
            Error::RecvTimeout(RecvTimeoutError::Timeout) => false,
            Error::Send => true,
            _ => {
                handle_error();
                error!("thread {:?} error {:?}", thread::current().name(), e);
                false
            }
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.t_cert_insert.join()
    }
}
