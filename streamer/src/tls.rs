use {
    ed25519_dalek::Verifier,
    rustls::{
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        crypto::{
            SharedSecret, SupportedKxGroup,
            verify_tls13_signature, ActiveKeyExchange, CryptoProvider, WebPkiSupportedAlgorithms,
        },
        PeerMisbehaved,
        pki_types::{
            AlgorithmIdentifier, CertificateDer, InvalidSignature, PrivateKeyDer, ServerName,
            SignatureVerificationAlgorithm, UnixTime,
        },
        server::danger::{ClientCertVerified, ClientCertVerifier},
        version::TLS13,
        CipherSuite, ClientConfig, DigitallySignedStruct, DistinguishedName, KeyLogFile,
        PeerIncompatible, ServerConfig, SignatureScheme,
    },
    std::sync::Arc,
    rand::rngs::OsRng,
};

// Boilerplate required to use ed25519_dalek for signature verification.

#[derive(Debug)]
struct DalekEd25519;
static DALEK_ED25519: &dyn SignatureVerificationAlgorithm = &DalekEd25519;
const ED25519_ALG_ID: AlgorithmIdentifier =
    AlgorithmIdentifier::from_slice(&[0x06, 0x03, 0x2b, 0x65, 0x70]);
impl SignatureVerificationAlgorithm for DalekEd25519 {
    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        ED25519_ALG_ID
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        ED25519_ALG_ID
    }

    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        let publickey =
            ed25519_dalek::PublicKey::from_bytes(public_key).map_err(|_| InvalidSignature)?;
        let signature =
            ed25519_dalek::Signature::try_from(signature).map_err(|_| InvalidSignature)?;
        publickey
            .verify(message, &signature)
            .map_err(|_| InvalidSignature)
    }
}

pub static TLS_SIGVERIFY_SCHEMES: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
    all: &[DALEK_ED25519],
    mapping: &[(SignatureScheme::ED25519, &[DALEK_ED25519])],
};

// Boilerplate required to use x25519_dalek for key exchange.

struct X25519State {
    secret: x25519_dalek::EphemeralSecret,
    pubkey: x25519_dalek::PublicKey,
}

impl ActiveKeyExchange for X25519State {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, rustls::Error> {
        let peer_array: [u8; 32] = peer
            .try_into()
            .map_err(|_| rustls::Error::from(PeerMisbehaved::InvalidKeyShare))?;
        let their_pub = x25519_dalek::PublicKey::from(peer_array);
        let shared_secret = self.secret.diffie_hellman(&their_pub);
        Ok(SharedSecret::from(&shared_secret.as_bytes()[..]))
    }

    fn pub_key(&self) -> &[u8] {
        self.pubkey.as_bytes()
    }

    fn group(&self) -> rustls::NamedGroup {
        X25519.name()
    }
}

#[derive(Debug)]
pub struct X25519Group;

impl SupportedKxGroup for X25519Group {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, rustls::Error> {
        let secret = x25519_dalek::EphemeralSecret::random_from_rng(OsRng);
        Ok(Box::new(X25519State {
            pubkey: (&secret).into(),
            secret,
        }))
    }

    fn name(&self) -> rustls::NamedGroup {
        rustls::NamedGroup::X25519
    }
}

pub static X25519: X25519Group = X25519Group;

// Assemble a CryptoProvider using ring and dalek

fn new_crypto_provider() -> CryptoProvider {
    let ring_provider = rustls::crypto::ring::default_provider();

    // Create minimal cipher suite list
    let mut aes128gcm = None;
    let mut chacha20poly1305 = None;
    for cipher_suite in ring_provider.cipher_suites {
        match cipher_suite.suite() {
            CipherSuite::TLS13_AES_128_GCM_SHA256 => aes128gcm = Some(cipher_suite),
            CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 => chacha20poly1305 = Some(cipher_suite),
            _ => {}
        }
    }
    let mut cipher_suites = Vec::with_capacity(2);
    if let Some(suite) = aes128gcm {
        cipher_suites.push(suite);
    }
    if let Some(suite) = chacha20poly1305 {
        cipher_suites.push(suite);
    }

    CryptoProvider {
        cipher_suites,
        kx_groups: vec![&X25519],
        key_provider: ring_provider.key_provider,
        secure_random: ring_provider.secure_random,
        signature_verification_algorithms: TLS_SIGVERIFY_SCHEMES,
    }
}

// Declare TLS 1.3 cert validation rules for p2p

#[derive(Debug)]
struct SkipServerVerification;

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl ServerCertVerifier for SkipServerVerification {
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &TLS_SIGVERIFY_SCHEMES)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        TLS_SIGVERIFY_SCHEMES.supported_schemes()
    }

    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
}

#[derive(Debug)]
struct SkipClientVerification;

impl SkipClientVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl ClientCertVerifier for SkipClientVerification {
    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer,
        _intermediates: &[CertificateDer],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &TLS_SIGVERIFY_SCHEMES)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        TLS_SIGVERIFY_SCHEMES.supported_schemes()
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }
}

pub fn new_server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    alpn: &[u8],
) -> Result<ServerConfig, rustls::Error> {
    let mut config = ServerConfig::builder_with_provider(Arc::new(new_crypto_provider()))
        .with_protocol_versions(&[&TLS13])?
        .with_client_cert_verifier(SkipClientVerification::new())
        .with_single_cert(vec![cert], key)?;
    config.alpn_protocols = vec![alpn.to_vec()];
    config.key_log = Arc::new(KeyLogFile::new());
    Ok(config)
}

pub fn new_client_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    alpn: &[u8],
) -> Result<ClientConfig, rustls::Error> {
    let mut config = ClientConfig::builder_with_provider(Arc::new(new_crypto_provider()))
        .with_protocol_versions(&[&TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_client_auth_cert(vec![cert], key)?;
    config.enable_early_data = true;
    config.alpn_protocols = vec![alpn.to_vec()];
    Ok(config)
}
