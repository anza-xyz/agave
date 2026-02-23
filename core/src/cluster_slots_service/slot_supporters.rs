use {
    crate::{consensus::Stake, replay_stage::DUPLICATE_THRESHOLD},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        hash::RandomState,
        ptr,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    },
};

//This is intended to be switched to solana_pubkey::PubkeyHasherBuilder
type PubkeyHasherBuilder = RandomState;
pub(crate) type IndexMap =
    HashMap</*node:*/ Pubkey, /*index*/ usize, PubkeyHasherBuilder>;

/// Amount of stake that needs to lock the slot for us to stop updating it.
/// This must be above `DUPLICATE_THRESHOLD` but also high enough to not starve
/// repair (as it will prefer nodes that have confirmed a slot)
const FREEZE_THRESHOLD: f64 = 0.9;

// whatever freeze threshold we should be above DUPLICATE_THRESHOLD in order to not break consensus.
// 1.1 margin is due to us not properly tracking epoch boundaries
static_assertions::const_assert!(FREEZE_THRESHOLD > DUPLICATE_THRESHOLD * 1.1);

/// Pubkey-stake map for nodes that have confirmed this slot.
/// Internally, this uses an index array that is shared for all instances
/// within an epoch.
#[derive(Debug)]
pub struct SlotSupporters {
    total_support: AtomicU64, // total support for this slot = supported_stakes.sum()
    total_stake: u64,         // total staked amount at this slot
    supporting_stakes: Vec<AtomicU64>, // amount of stake per validator that has confirmed this slot
    pubkey_to_index_map: Arc<IndexMap>, // map from pubkey to node index in supporting_stakes vector
}

fn repeat_atomic_u64(count: usize) -> impl Iterator<Item = AtomicU64> {
    std::iter::repeat_with(|| AtomicU64::new(0)).take(count)
}

impl SlotSupporters {
    pub(crate) fn memory_usage(&self) -> usize {
        self.supporting_stakes.capacity() * std::mem::size_of::<AtomicU64>()
    }

    #[inline]
    pub(crate) fn set_support_by_index(&self, index: usize, stake: Stake) {
        let old = self.supporting_stakes[index].swap(stake, Ordering::Relaxed);
        if stake > old {
            self.total_support.fetch_add(stake - old, Ordering::Relaxed);
        }
    }
    /// Current support for this slot (sum of supporting stakes)
    #[inline]
    pub(crate) fn total_support(&self) -> Stake {
        self.total_support
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Total staked amount for this slot
    ///   #[inline]
    pub(crate) fn total_stake(&self) -> Stake {
        self.total_stake
    }
    #[inline]
    pub(crate) fn is_frozen(&self) -> bool {
        let slot_weight_f64 = self.total_support() as f64;

        // once slot is confirmed beyond `FREEZE_THRESHOLD` we should not need
        // to update the stakes for it anymore
        slot_weight_f64 / self.total_stake as f64 > FREEZE_THRESHOLD
    }

    #[inline]
    pub(crate) fn set_support_by_pubkey(&self, pubkey: &Pubkey, stake: Stake) -> Result<(), ()> {
        let Some(idx) = self.pubkey_to_index_map.get(pubkey) else {
            return Err(());
        };
        self.set_support_by_index(*idx, stake);
        Ok(())
    }

    #[inline]
    pub(crate) fn get_support_by_index(&self, index: usize) -> Option<Stake> {
        Some(self.supporting_stakes.get(index)?.load(Ordering::Relaxed))
    }
    #[inline]
    pub(crate) fn get_support_by_pubkey(&self, key: &Pubkey) -> Option<Stake> {
        let index = self.pubkey_to_index_map.get(key)?;
        self.get_support_by_index(*index)
    }

    pub(crate) fn new(total_stake: Stake, index_map: Arc<IndexMap>) -> Self {
        Self {
            total_support: AtomicU64::new(0),
            total_stake,
            supporting_stakes: Vec::from_iter(repeat_atomic_u64(index_map.len())),
            pubkey_to_index_map: index_map,
        }
    }
    pub(crate) fn new_blank() -> Self {
        Self {
            total_support: AtomicU64::new(0),
            total_stake: 0,
            supporting_stakes: vec![],
            pubkey_to_index_map: Arc::new(HashMap::default()),
        }
    }
    pub(crate) fn is_blank(&self) -> bool {
        self.total_stake == 0
    }
    // recycles the object for new slot. If index_map is not provided,
    // the old one will be reused (which is fine within the epoch)
    pub(crate) fn recycle(mut self, total_stake: Stake, index_map: &Arc<IndexMap>) -> Self {
        self.total_stake = total_stake;
        self.total_support.store(0, Ordering::Relaxed);
        let same_epoch = ptr::eq(
            Arc::as_ptr(index_map),
            Arc::as_ptr(&self.pubkey_to_index_map),
        );
        if !same_epoch {
            let old_len = self.supporting_stakes.len();
            let new_len = index_map.len();
            if new_len < old_len * 2 {
                // if new length is much less than allocation, reallocate
                self.supporting_stakes = Vec::from_iter(repeat_atomic_u64(new_len));
            } else {
                // cut vec to length if needed
                self.supporting_stakes.truncate(new_len);
                // reset all old elements to zero
                self.supporting_stakes
                    .iter_mut()
                    .for_each(|v| v.store(0, Ordering::Relaxed));
                if self.supporting_stakes.len() < new_len {
                    let num_missing = new_len - self.supporting_stakes.len();
                    self.supporting_stakes
                        .extend(repeat_atomic_u64(num_missing));
                }
            }
            self.pubkey_to_index_map = index_map.clone();
        } else {
            self.supporting_stakes
                .iter_mut()
                .for_each(|v| v.store(0, Ordering::Relaxed));
        }
        self
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Pubkey, Stake)> {
        self.pubkey_to_index_map
            .iter()
            .map(|(pk, &idx)| (pk, self.get_support_by_index(idx).unwrap()))
    }

    pub fn keys(&self) -> impl Iterator<Item = &Pubkey> {
        self.pubkey_to_index_map.iter().filter_map(|(pk, &idx)| {
            if self.get_support_by_index(idx).unwrap() > 0 {
                Some(pk)
            } else {
                None
            }
        })
    }
}
