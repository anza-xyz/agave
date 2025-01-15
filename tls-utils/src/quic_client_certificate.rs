use {
    crate::new_dummy_x509_certificate,
    rustls::pki_types::{CertificateDer, PrivateKeyDer},
    solana_keypair::Keypair,
};

pub struct QuicClientCertificate {
    pub certificate: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl QuicClientCertificate {
    pub fn new(keypair: Option<&Keypair>) -> Self {
        let keypair = if let Some(keypair) = keypair {
            keypair
        } else {
            &Keypair::new()
        };
        let (certificate, key) = new_dummy_x509_certificate(keypair);
        Self { certificate, key }
    }
}
