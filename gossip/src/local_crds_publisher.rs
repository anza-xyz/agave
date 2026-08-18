use {
    crate::{
        crds::{CrdsStore, InsertOutcome, LocalBatchOutcome},
        crds_value::{CrdsValue, CrdsValueLabel},
    },
    indexmap::IndexMap,
    std::sync::Mutex,
};

/// Owns the mechanics for publishing values produced by the local node.
///
/// Callers decide whether a value should be published immediately or queued
/// for the next push round; locking and CRDS merge routing stay here.
#[derive(Default)]
pub(crate) struct LocalCrdsPublisher {
    pending: Mutex<Vec<CrdsValue>>,
}

impl LocalCrdsPublisher {
    pub(crate) fn enqueue(&self, value: CrdsValue) {
        self.pending.lock().unwrap().push(value);
    }

    pub(crate) fn publish(&self, crds: &CrdsStore, value: CrdsValue, now: u64) -> InsertOutcome {
        crds.insert_local(value, now)
    }

    pub(crate) fn publish_batch(
        &self,
        crds: &CrdsStore,
        values: Vec<CrdsValue>,
        now: u64,
    ) -> LocalBatchOutcome {
        let values = values
            .into_iter()
            .fold(
                IndexMap::<CrdsValueLabel, CrdsValue>::new(),
                |mut values, value| {
                    values.insert(value.label(), value);
                    values
                },
            )
            .into_values()
            .collect();
        crds.insert_local_batch(values, now)
    }

    pub(crate) fn flush(&self, crds: &CrdsStore, now: u64) {
        let values = std::mem::take(&mut *self.pending.lock().unwrap());
        for value in values {
            // A later revision of the same label may already have reached
            // CRDS. Duplicate and stale queued values are expected here.
            let _ = self.publish(crds, value, now);
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
