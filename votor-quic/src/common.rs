use {quinn::Connection, solana_pubkey::Pubkey, solana_tls_utils::get_pubkey_from_tls_certificate};

pub(crate) const ALPN_QUIC_VOTOR: &[u8] = b"votor";

pub(crate) fn get_remote_pubkey(connection: &Connection) -> Option<Pubkey> {
    // Use the client cert only if it is self signed and the chain length is 1.
    connection
        .peer_identity()?
        .downcast::<Vec<rustls::pki_types::CertificateDer>>()
        .ok()
        .filter(|certs| certs.len() == 1)?
        .first()
        .and_then(get_pubkey_from_tls_certificate)
}
