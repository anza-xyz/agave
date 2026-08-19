use {
    crate::{contact_info::ContactInfo, crds_data::CrdsData, crds_value::CrdsValue},
    arc_swap::ArcSwap,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::sync::{Arc, Mutex},
};

/// Keypair and contact record for the same pubkey.
struct IdentitySnapshot {
    keypair: Arc<Keypair>,
    contact_info: ContactInfo,
}

/// Atomically publishes identity snapshots and serializes updates.
pub(crate) struct GossipIdentity {
    snapshot: ArcSwap<IdentitySnapshot>,
    update_lock: Mutex<()>,
}

impl GossipIdentity {
    pub(crate) fn new(contact_info: ContactInfo, keypair: Arc<Keypair>) -> Self {
        assert_eq!(contact_info.pubkey(), &keypair.pubkey());
        Self {
            snapshot: ArcSwap::from_pointee(IdentitySnapshot {
                keypair,
                contact_info,
            }),
            update_lock: Mutex::new(()),
        }
    }

    pub(crate) fn id(&self) -> Pubkey {
        self.snapshot.load().keypair.pubkey()
    }

    pub(crate) fn keypair(&self) -> Arc<Keypair> {
        Arc::clone(&self.snapshot.load().keypair)
    }

    pub(crate) fn set_keypair(&self, keypair: Arc<Keypair>) {
        let _update_lock = self.update_lock.lock().unwrap();
        let snapshot = self.snapshot.load_full();
        let mut contact_info = snapshot.contact_info.clone();
        contact_info.hot_swap_pubkey(keypair.pubkey());
        self.snapshot.store(Arc::new(IdentitySnapshot {
            keypair,
            contact_info,
        }));
    }

    pub(crate) fn contact_info(&self) -> ContactInfo {
        self.snapshot.load().contact_info.clone()
    }

    pub(crate) fn shred_version(&self) -> u16 {
        self.snapshot.load().contact_info.shred_version()
    }

    pub(crate) fn update_contact_info<R>(&self, update: impl FnOnce(&mut ContactInfo) -> R) -> R {
        let _update_lock = self.update_lock.lock().unwrap();
        let snapshot = self.snapshot.load_full();
        let mut contact_info = snapshot.contact_info.clone();
        let result = update(&mut contact_info);
        self.snapshot.store(Arc::new(IdentitySnapshot {
            keypair: Arc::clone(&snapshot.keypair),
            contact_info,
        }));
        result
    }

    /// Refreshes, signs, and publishes the contact record from one snapshot.
    pub(crate) fn refreshed_crds_value(&self, now: u64) -> CrdsValue {
        let _update_lock = self.update_lock.lock().unwrap();
        let snapshot = self.snapshot.load_full();
        let mut contact_info = snapshot.contact_info.clone();
        contact_info.set_wallclock(now);
        let keypair = Arc::clone(&snapshot.keypair);
        self.snapshot.store(Arc::new(IdentitySnapshot {
            keypair: Arc::clone(&keypair),
            contact_info: contact_info.clone(),
        }));
        CrdsValue::new(CrdsData::ContactInfo(contact_info), &keypair)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::contact_info::ContactInfo, solana_keypair::signable::Signable,
        std::sync::Barrier,
    };

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

    #[test]
    fn test_refreshed_value_uses_coherent_identity() {
        let keypair = Arc::new(Keypair::new());
        let identity = Arc::new(GossipIdentity::new(
            ContactInfo::new_localhost(&keypair.pubkey(), 1),
            keypair,
        ));
        let barrier = Arc::new(Barrier::new(2));
        let thread = {
            let identity = Arc::clone(&identity);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..100 {
                    identity.set_keypair(Arc::new(Keypair::new()));
                }
            })
        };
        barrier.wait();
        for now in 2..102 {
            let value = identity.refreshed_crds_value(now);
            assert!(value.verify());
            assert_eq!(value.pubkey(), *value.contact_info().unwrap().pubkey());
        }
        thread.join().unwrap();
    }
}
