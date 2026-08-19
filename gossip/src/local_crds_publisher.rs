use {
    crate::{crds::CrdsStore, crds_value::CrdsValue},
    std::sync::Mutex,
};

/// Queue of values produced by this node that are published on the next round.
///
/// Values that should reach CRDS immediately go straight to
/// [`CrdsStore::insert_local`]; this type only owns the deferral.
#[derive(Default)]
pub(crate) struct LocalCrdsPublisher {
    pending: Mutex<Vec<CrdsValue>>,
}

impl LocalCrdsPublisher {
    pub(crate) fn enqueue(&self, value: CrdsValue) {
        self.pending.lock().unwrap().push(value);
    }

    pub(crate) fn flush(&self, crds: &CrdsStore, now: u64) {
        let values = std::mem::take(&mut *self.pending.lock().unwrap());
        for value in values {
            // A later revision of the same label may already have reached
            // CRDS. Duplicate and stale queued values are expected here.
            let _ = crds.insert_local(value, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{contact_info::ContactInfo, crds_data::CrdsData},
        solana_pubkey::Pubkey,
    };

    #[test]
    fn test_flush_drains_pending_values() {
        let publisher = LocalCrdsPublisher::default();
        let crds = CrdsStore::default();
        let pubkey = Pubkey::new_unique();
        let value = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(&pubkey, 1)));
        let label = value.label();
        publisher.enqueue(value);

        publisher.flush(&crds, 1);
        assert!(crds.read().get::<&CrdsValue>(&label).is_some());

        crds.write().remove(&label, 2);
        publisher.flush(&crds, 3);
        assert!(crds.read().get::<&CrdsValue>(&label).is_none());
    }
}
