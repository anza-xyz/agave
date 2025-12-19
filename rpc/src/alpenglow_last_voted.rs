// This is a temporay work around to correctly report last votes of peers in Alpenglow
// before we have the certs in banks.
use {
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::RwLock},
};

// We should only have 50 nodes in test cluster. By the time we move to testnet we
// will have the real thing implemented.
const MAX_ENTRIES: usize = 2000;

#[derive(Default)]
pub struct AlpenglowLastVoted {
    last_voted_map: RwLock<HashMap<Pubkey, Slot>>,
}

impl AlpenglowLastVoted {
    pub fn update_last_voted(&self, verified_votes_by_pubkey: &HashMap<Pubkey, Slot>) {
        let mut map = self.last_voted_map.write().unwrap();
        for (pubkey, largest_slot) in verified_votes_by_pubkey {
            let Some(entry) = map.get_mut(pubkey) else {
                if map.len() >= MAX_ENTRIES {
                    warn!("AlpenglowLastVoted map reached max entries, removing oldest entry");
                    let oldest_key = map
                        .iter()
                        .min_by_key(|(_, slot)| *slot)
                        .map(|(k, _)| *k)
                        .expect("Failed to find oldest entry in AlpenglowLastVoted map");

                    map.remove(&oldest_key);
                }
                map.insert(*pubkey, *largest_slot);
                continue;
            };
            *entry = (*entry).max(*largest_slot);
        }
    }

    pub fn get_last_voted(&self, pubkey: &Pubkey) -> Option<Slot> {
        self.last_voted_map.read().unwrap().get(pubkey).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpenglow_last_voted() {
        let alpenglow_last_voted = AlpenglowLastVoted::default();
        let pubkey1 = Pubkey::new_unique();
        let pubkey2 = Pubkey::new_unique();
        alpenglow_last_voted.update_last_voted(&HashMap::from([(pubkey1, 1), (pubkey2, 2)]));
        assert_eq!(alpenglow_last_voted.get_last_voted(&pubkey1), Some(1));
        assert_eq!(alpenglow_last_voted.get_last_voted(&pubkey2), Some(2));
        assert_eq!(
            alpenglow_last_voted.get_last_voted(&Pubkey::new_unique()),
            None
        );
    }
}
