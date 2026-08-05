use {
    agave_votor_messages::certificate::CertificateType, parking_lot::RwLock, solana_clock::Slot,
    std::collections::HashSet,
};

/// A simple container that allows the consensus pool to communicate with the bls sigverifier
/// which certs it has already generated and therefore does not need anymore.
#[derive(Default)]
pub struct GeneratedCertTypes(RwLock<HashSet<CertificateType>>);

impl GeneratedCertTypes {
    /// Returns `true` if the pool already has the `cert_type`.
    pub fn has_cert(&self, cert_type: &CertificateType) -> bool {
        self.0.read().contains(cert_type)
    }

    /// Inserts the `cert_type` into the container.
    pub fn insert_cert(&self, cert_type: CertificateType) {
        self.0.write().insert(cert_type);
    }

    /// Prunes the container, it drops all certs older than `root_slot` as they are no longer needed.
    pub fn prune(&self, root_slot: Slot) {
        self.0.write().retain(|c| c.slot() >= root_slot);
    }
}
