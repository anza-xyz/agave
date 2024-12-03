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
    pub fn new(keypair: Option<&Keypair>) -> Self {
        if let Some(keypair) = keypair {
            let (certificate, key) = new_dummy_x509_certificate(keypair);
            Self { certificate, key }
        } else {
            let (certificate, key) = new_dummy_x509_certificate(&Keypair::new());
            Self { certificate, key }
        }
    }
}
