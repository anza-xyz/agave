use {
    crate::{contact_info::ContactInfo, crds_data::CrdsData, crds_value::CrdsValue},
    arc_swap::ArcSwap,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::sync::{Arc, RwLock},
};

/// Owns the signing identity and the contact record advertised for it.
pub(crate) struct GossipIdentity {
    keypair: ArcSwap<Keypair>,
    contact_info: RwLock<ContactInfo>,
}

impl GossipIdentity {
    pub(crate) fn new(contact_info: ContactInfo, keypair: Arc<Keypair>) -> Self {
        assert_eq!(contact_info.pubkey(), &keypair.pubkey());
        Self {
            keypair: ArcSwap::from(keypair),
            contact_info: RwLock::new(contact_info),
        }
    }

    pub(crate) fn id(&self) -> Pubkey {
        self.keypair.load().pubkey()
    }

    pub(crate) fn keypair(&self) -> Arc<Keypair> {
        self.keypair.load_full()
    }

    pub(crate) fn set_keypair(&self, keypair: Arc<Keypair>) {
        let id = keypair.pubkey();
        self.keypair.store(keypair);
        self.contact_info.write().unwrap().hot_swap_pubkey(id);
    }

    pub(crate) fn contact_info(&self) -> ContactInfo {
        self.contact_info.read().unwrap().clone()
    }

    pub(crate) fn shred_version(&self) -> u16 {
        self.contact_info.read().unwrap().shred_version()
    }

    pub(crate) fn update_contact_info<R>(&self, update: impl FnOnce(&mut ContactInfo) -> R) -> R {
        update(&mut self.contact_info.write().unwrap())
    }

    pub(crate) fn refreshed_crds_value(&self, now: u64) -> CrdsValue {
        let keypair = self.keypair();
        let contact_info = self.update_contact_info(|contact_info| {
            contact_info.set_wallclock(now);
            contact_info.clone()
        });
        CrdsValue::new(CrdsData::ContactInfo(contact_info), &keypair)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::contact_info::ContactInfo, solana_keypair::signable::Signable};

    #[test]
    fn test_keypair_rotation_updates_contact_info() {
        let keypair = Arc::new(Keypair::new());
        let identity =
            GossipIdentity::new(ContactInfo::new_localhost(&keypair.pubkey(), 1), keypair);
        let replacement = Arc::new(Keypair::new());
        identity.set_keypair(replacement.clone());

        assert_eq!(identity.id(), replacement.pubkey());
        assert_eq!(identity.contact_info().pubkey(), &replacement.pubkey());
        assert!(identity.refreshed_crds_value(2).verify());
    }
}
