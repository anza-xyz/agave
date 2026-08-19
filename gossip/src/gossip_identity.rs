use {
    crate::{contact_info::ContactInfo, crds_data::CrdsData, crds_value::CrdsValue},
    arc_swap::ArcSwap,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::sync::{Arc, Mutex},
};

/// Keypair and contact record for the same pubkey.
#[derive(Clone)]
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

    /// Serializes an update against the published snapshot and republishes it,
    /// so concurrent socket updates and identity rotation cannot lose each
    /// other's changes.
    fn mutate<R>(&self, update: impl FnOnce(&mut IdentitySnapshot) -> R) -> R {
        let _update_lock = self.update_lock.lock().unwrap();
        let mut next = (*self.snapshot.load_full()).clone();
        let result = update(&mut next);
        self.snapshot.store(Arc::new(next));
        result
    }

    pub(crate) fn set_keypair(&self, keypair: Arc<Keypair>) {
        self.mutate(|snapshot| {
            snapshot.contact_info.hot_swap_pubkey(keypair.pubkey());
            snapshot.keypair = keypair;
        })
    }

    pub(crate) fn contact_info(&self) -> ContactInfo {
        self.snapshot.load().contact_info.clone()
    }

    pub(crate) fn shred_version(&self) -> u16 {
        self.snapshot.load().contact_info.shred_version()
    }

    pub(crate) fn update_contact_info<R>(&self, update: impl FnOnce(&mut ContactInfo) -> R) -> R {
        self.mutate(|snapshot| update(&mut snapshot.contact_info))
    }

    /// Refreshes, signs, and publishes the contact record from one snapshot.
    pub(crate) fn refreshed_crds_value(&self, now: u64) -> CrdsValue {
        self.mutate(|snapshot| {
            snapshot.contact_info.set_wallclock(now);
            CrdsValue::new(
                CrdsData::ContactInfo(snapshot.contact_info.clone()),
                &snapshot.keypair,
            )
        })
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
