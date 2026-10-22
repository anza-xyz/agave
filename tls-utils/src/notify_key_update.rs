use solana_keypair::Keypair;

/// [`NotifyKeyUpdate`] is a trait used for updating the certificate used for QUIC connections.
///
/// When validator receives signal to update its identity through the admin_rpc, we need to change
/// the keypair used for QUIC connections. This trait provides an interface for that.
pub trait NotifyKeyUpdate {
    fn update_key(&self, key: &Keypair) -> Result<(), Box<dyn core::error::Error>>;
}
