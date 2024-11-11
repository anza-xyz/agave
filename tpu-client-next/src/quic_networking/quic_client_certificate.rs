use {
    rustls::pki_types::{CertificateDer, PrivateKeyDer},
    solana_sdk::signature::Keypair,
    solana_streamer::tls_certificates::new_dummy_x509_certificate,
};

pub struct QuicClientCertificate {
    pub certificate: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl QuicClientCertificate {
    pub fn with_option(keypair: Option<&Keypair>) -> Self {
        if let Some(keypair) = keypair {
            QuicClientCertificate::new(keypair)
        } else {
            QuicClientCertificate::new(&Keypair::new())
        }
    }

    pub fn new(keypair: &Keypair) -> Self {
        let (certificate, key) = new_dummy_x509_certificate(keypair);
        Self { certificate, key }
    }
}