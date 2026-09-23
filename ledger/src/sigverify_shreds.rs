#![allow(clippy::implicit_hasher)]
use {
    crate::shred::{Admissible, AnyShred, Verified},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_nohash_hasher::BuildNoHashHasher,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{collections::HashMap, sync::RwLock},
};

pub type LruCache = lazy_lru::LruCache<(Signature, Pubkey, Hash), ()>;

pub type SlotPubkeys = HashMap<Slot, Pubkey, BuildNoHashHasher<Slot>>;

pub fn verify_shred(
    shred: AnyShred<Admissible>,
    slot_leaders: &SlotPubkeys,
    cache: &RwLock<LruCache>,
) -> Option<AnyShred<Verified>> {
    let leader = slot_leaders.get(&shred.slot())?;
    shred
        .verify_with(leader, |signature, pubkey, root| {
            let key = (*signature, *pubkey, *root);
            if cache.read().unwrap().get(&key).is_some() {
                return true;
            }
            if agave_shred::verify(signature, pubkey, root) {
                cache.write().unwrap().put(key, ());
                true
            } else {
                false
            }
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            shred::{ProcessShredsStats, Shred, parse_turbine},
            shredder::Shredder,
        },
        agave_shred::policy::AdmissionPolicy,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_signer::Signer,
    };

    fn admissible(shred: &Shred) -> AnyShred<Admissible> {
        let policy = AdmissionPolicy {
            shred_version: shred.version(),
            root: shred.slot().saturating_sub(1),
            max_slot: shred.slot() + 1,
            max_data_shreds_per_slot: 32_768,
            max_code_shreds_per_slot: 32_768,
        };
        parse_turbine(shred.bytes().clone())
            .unwrap()
            .check_policy(&policy)
            .unwrap()
    }

    #[test]
    fn test_verify_shred() {
        let slot = 0xdead_c0de;
        let cache = RwLock::new(LruCache::new(128));
        let shredder = Shredder::new(slot, slot - 1, 0, 0).unwrap();
        let keypair = Keypair::new();
        let (mut shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &[],
            true,
            Hash::default(),
            0,
            &mut ProcessShredsStats::default(),
        );
        let shred = shreds.pop().unwrap();

        let leader_slots: SlotPubkeys = [(slot, keypair.pubkey())].into_iter().collect();
        assert!(verify_shred(admissible(&shred), &leader_slots, &cache).is_some());
        assert!(verify_shred(admissible(&shred), &leader_slots, &cache).is_some());

        let wrong_keypair = Keypair::new();
        let leader_slots: SlotPubkeys = [(slot, wrong_keypair.pubkey())].into_iter().collect();
        assert!(verify_shred(admissible(&shred), &leader_slots, &cache).is_none());

        let leader_slots: SlotPubkeys = HashMap::default();
        assert!(verify_shred(admissible(&shred), &leader_slots, &cache).is_none());
    }
}
