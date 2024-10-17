//! Collection of reserved account keys that cannot be write-locked by transactions.
//! New reserved account keys may be added as long as they specify a feature
//! gate that transitions the key into read-only at an epoch boundary.
#![no_std]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#[cfg(all(feature = "std", not(target_os = "solana")))]
extern crate std;
#[cfg(all(feature = "std", not(target_os = "solana")))]
use {
    solana_feature_set::{self as feature_set, FeatureSet},
    solana_pubkey::Pubkey,
    std::collections::{HashMap, HashSet},
};

pub mod address_lookup_table {
    solana_pubkey::declare_id!("AddressLookupTab1e1111111111111111111111111");
}

pub mod bpf_loader {
    solana_pubkey::declare_id!("BPFLoader2111111111111111111111111111111111");
}

pub mod bpf_loader_deprecated {
    solana_pubkey::declare_id!("BPFLoader1111111111111111111111111111111111");
}

pub mod bpf_loader_upgradeable {
    solana_pubkey::declare_id!("BPFLoaderUpgradeab1e11111111111111111111111");
}

pub mod compute_budget {
    solana_pubkey::declare_id!("ComputeBudget111111111111111111111111111111");
}

pub mod config {
    solana_pubkey::declare_id!("Config1111111111111111111111111111111111111");
}

pub mod ed25519_program {
    solana_pubkey::declare_id!("Ed25519SigVerify111111111111111111111111111");
}

pub mod feature {
    solana_pubkey::declare_id!("Feature111111111111111111111111111111111111");
}

pub mod loader_v4 {
    solana_pubkey::declare_id!("LoaderV411111111111111111111111111111111111");
}

pub mod native_loader {
    solana_pubkey::declare_id!("NativeLoader1111111111111111111111111111111");
}

pub mod secp256k1_program {
    solana_pubkey::declare_id!("KeccakSecp256k11111111111111111111111111111");
}

pub mod stake {
    pub mod config {
        solana_pubkey::declare_deprecated_id!("StakeConfig11111111111111111111111111111111");
    }
    pub mod program {
        solana_pubkey::declare_id!("Stake11111111111111111111111111111111111111");
    }
}

pub mod system_program {
    solana_pubkey::declare_id!("11111111111111111111111111111111");
}

pub mod vote {
    solana_pubkey::declare_id!("Vote111111111111111111111111111111111111111");
}

pub mod sysvar {
    // Owner pubkey for sysvar accounts
    solana_pubkey::declare_id!("Sysvar1111111111111111111111111111111111111");
    pub mod clock {
        solana_pubkey::declare_id!("SysvarC1ock11111111111111111111111111111111");
    }
    pub mod epoch_rewards {
        solana_pubkey::declare_id!("SysvarEpochRewards1111111111111111111111111");
    }
    pub mod epoch_schedule {
        solana_pubkey::declare_id!("SysvarEpochSchedu1e111111111111111111111111");
    }
    pub mod fees {
        solana_pubkey::declare_id!("SysvarFees111111111111111111111111111111111");
    }
    pub mod instructions {
        solana_pubkey::declare_id!("Sysvar1nstructions1111111111111111111111111");
    }
    pub mod last_restart_slot {
        solana_pubkey::declare_id!("SysvarLastRestartS1ot1111111111111111111111");
    }
    pub mod recent_blockhashes {
        solana_pubkey::declare_id!("SysvarRecentB1ockHashes11111111111111111111");
    }
    pub mod rent {
        solana_pubkey::declare_id!("SysvarRent111111111111111111111111111111111");
    }
    pub mod rewards {
        solana_pubkey::declare_id!("SysvarRewards111111111111111111111111111111");
    }
    pub mod slot_hashes {
        solana_pubkey::declare_id!("SysvarS1otHashes111111111111111111111111111");
    }
    pub mod slot_history {
        solana_pubkey::declare_id!("SysvarS1otHistory11111111111111111111111111");
    }
    pub mod stake_history {
        solana_pubkey::declare_id!("SysvarStakeHistory1111111111111111111111111");
    }
}

pub mod zk_token_proof_program {
    solana_pubkey::declare_id!("ZkTokenProof1111111111111111111111111111111");
}

pub mod zk_elgamal_proof_program {
    solana_pubkey::declare_id!("ZkE1Gama1Proof11111111111111111111111111111");
}

// ReservedAccountKeys is not serialized into or deserialized from bank
// snapshots but the bank requires this trait to be implemented anyways.
#[cfg(feature = "frozen-abi")]
impl ::solana_frozen_abi::abi_example::AbiExample for ReservedAccountKeys {
    fn example() -> Self {
        // ReservedAccountKeys is not Serialize so just rely on Default.
        ReservedAccountKeys::default()
    }
}

#[cfg(all(feature = "std", not(target_os = "solana")))]
/// `ReservedAccountKeys` holds the set of currently active/inactive
/// account keys that are reserved by the protocol and may not be write-locked
/// during transaction processing.
#[derive(Debug, Clone, PartialEq)]
pub struct ReservedAccountKeys {
    /// Set of currently active reserved account keys
    pub active: HashSet<Pubkey>,
    /// Set of currently inactive reserved account keys that will be moved to the
    /// active set when their feature id is activated
    inactive: HashMap<Pubkey, Pubkey>,
}

#[cfg(all(feature = "std", not(target_os = "solana")))]
impl Default for ReservedAccountKeys {
    fn default() -> Self {
        Self::new(&RESERVED_ACCOUNTS)
    }
}

#[cfg(all(feature = "std", not(target_os = "solana")))]
impl ReservedAccountKeys {
    /// Compute a set of active / inactive reserved account keys from a list of
    /// keys with a designated feature id. If a reserved account key doesn't
    /// designate a feature id, it's already activated and should be inserted
    /// into the active set. If it does have a feature id, insert the key and
    /// its feature id into the inactive map.
    fn new(reserved_accounts: &[ReservedAccount]) -> Self {
        Self {
            active: reserved_accounts
                .iter()
                .filter(|reserved| reserved.feature_id.is_none())
                .map(|reserved| reserved.key)
                .collect(),
            inactive: reserved_accounts
                .iter()
                .filter_map(|ReservedAccount { key, feature_id }| {
                    feature_id.as_ref().map(|feature_id| (*key, *feature_id))
                })
                .collect(),
        }
    }

    /// Compute a set with all reserved keys active, regardless of whether their
    /// feature was activated. This is not to be used by the runtime. Useful for
    /// off-chain utilities that need to filter out reserved accounts.
    pub fn new_all_activated() -> Self {
        Self {
            active: Self::all_keys_iter().copied().collect(),
            inactive: HashMap::default(),
        }
    }

    /// Returns whether the specified key is reserved
    pub fn is_reserved(&self, key: &Pubkey) -> bool {
        self.active.contains(key)
    }

    /// Move inactive reserved account keys to the active set if their feature
    /// is active.
    pub fn update_active_set(&mut self, feature_set: &FeatureSet) {
        self.inactive.retain(|reserved_key, feature_id| {
            if feature_set.is_active(feature_id) {
                self.active.insert(*reserved_key);
                false
            } else {
                true
            }
        });
    }

    /// Return an iterator over all active / inactive reserved keys. This is not
    /// to be used by the runtime. Useful for off-chain utilities that need to
    /// filter out reserved accounts.
    pub fn all_keys_iter() -> impl Iterator<Item = &'static Pubkey> {
        RESERVED_ACCOUNTS
            .iter()
            .map(|reserved_key| &reserved_key.key)
    }

    /// Return an empty set of reserved keys for visibility when using in
    /// tests where the dynamic reserved key set is not available
    pub fn empty_key_set() -> HashSet<Pubkey> {
        HashSet::default()
    }
}

#[cfg(all(feature = "std", not(target_os = "solana")))]
/// `ReservedAccount` represents a reserved account that will not be
/// write-lockable by transactions. If a feature id is set, the account will
/// become read-only only after the feature has been activated.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct ReservedAccount {
    key: Pubkey,
    feature_id: Option<Pubkey>,
}

#[cfg(all(feature = "std", not(target_os = "solana")))]
impl ReservedAccount {
    fn new_pending(key: Pubkey, feature_id: Pubkey) -> Self {
        Self {
            key,
            feature_id: Some(feature_id),
        }
    }

    fn new_active(key: Pubkey) -> Self {
        Self {
            key,
            feature_id: None,
        }
    }
}

#[cfg(all(feature = "std", not(target_os = "solana")))]
// New reserved accounts should be added in alphabetical order and must specify
// a feature id for activation. Reserved accounts cannot be removed from this
// list without breaking consensus.
lazy_static::lazy_static! {
    static ref RESERVED_ACCOUNTS: std::vec::Vec<ReservedAccount> = [
        // builtin programs
        ReservedAccount::new_pending(address_lookup_table::id(), feature_set::add_new_reserved_account_keys::id()),
        ReservedAccount::new_active(bpf_loader::id()),
        ReservedAccount::new_active(bpf_loader_deprecated::id()),
        ReservedAccount::new_active(bpf_loader_upgradeable::id()),
        ReservedAccount::new_pending(compute_budget::id(), feature_set::add_new_reserved_account_keys::id()),
        ReservedAccount::new_active(config::id()),
        ReservedAccount::new_pending(ed25519_program::id(), feature_set::add_new_reserved_account_keys::id()),
        ReservedAccount::new_active(feature::id()),
        ReservedAccount::new_pending(loader_v4::id(), feature_set::add_new_reserved_account_keys::id()),
        ReservedAccount::new_pending(secp256k1_program::id(), feature_set::add_new_reserved_account_keys::id()),
        #[allow(deprecated)]
        ReservedAccount::new_active(stake::config::id()),
        ReservedAccount::new_active(stake::program::id()),
        ReservedAccount::new_active(system_program::id()),
        ReservedAccount::new_active(vote::id()),
        ReservedAccount::new_pending(zk_elgamal_proof_program::id(), feature_set::add_new_reserved_account_keys::id()),
        ReservedAccount::new_pending(zk_token_proof_program::id(), feature_set::add_new_reserved_account_keys::id()),

        // sysvars
        ReservedAccount::new_active(sysvar::clock::id()),
        ReservedAccount::new_pending(sysvar::epoch_rewards::id(), feature_set::add_new_reserved_account_keys::id()),
        ReservedAccount::new_active(sysvar::epoch_schedule::id()),
        #[allow(deprecated)]
        ReservedAccount::new_active(sysvar::fees::id()),
        ReservedAccount::new_active(sysvar::instructions::id()),
        ReservedAccount::new_pending(sysvar::last_restart_slot::id(), feature_set::add_new_reserved_account_keys::id()),
        #[allow(deprecated)]
        ReservedAccount::new_active(sysvar::recent_blockhashes::id()),
        ReservedAccount::new_active(sysvar::rent::id()),
        ReservedAccount::new_active(sysvar::rewards::id()),
        ReservedAccount::new_active(sysvar::slot_hashes::id()),
        ReservedAccount::new_active(sysvar::slot_history::id()),
        ReservedAccount::new_active(sysvar::stake_history::id()),

        // other
        ReservedAccount::new_active(native_loader::id()),
        ReservedAccount::new_pending(sysvar::id(), feature_set::add_new_reserved_account_keys::id()),
    ].to_vec();
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]
    use {
        super::*,
        solana_program::{message::legacy::BUILTIN_PROGRAMS_KEYS, sysvar::ALL_IDS},
    };

    #[test]
    fn test_is_reserved() {
        let feature_id = Pubkey::new_unique();
        let active_reserved_account = ReservedAccount::new_active(Pubkey::new_unique());
        let pending_reserved_account =
            ReservedAccount::new_pending(Pubkey::new_unique(), feature_id);
        let reserved_account_keys =
            ReservedAccountKeys::new(&[active_reserved_account, pending_reserved_account]);

        assert!(
            reserved_account_keys.is_reserved(&active_reserved_account.key),
            "active reserved accounts should be inserted into the active set"
        );
        assert!(
            !reserved_account_keys.is_reserved(&pending_reserved_account.key),
            "pending reserved accounts should NOT be inserted into the active set"
        );
    }

    #[test]
    fn test_update_active_set() {
        let feature_ids = [Pubkey::new_unique(), Pubkey::new_unique()];
        let active_reserved_key = Pubkey::new_unique();
        let pending_reserved_keys = [Pubkey::new_unique(), Pubkey::new_unique()];
        let reserved_accounts = std::vec![
            ReservedAccount::new_active(active_reserved_key),
            ReservedAccount::new_pending(pending_reserved_keys[0], feature_ids[0]),
            ReservedAccount::new_pending(pending_reserved_keys[1], feature_ids[1]),
        ];

        let mut reserved_account_keys = ReservedAccountKeys::new(&reserved_accounts);
        assert!(reserved_account_keys.is_reserved(&active_reserved_key));
        assert!(!reserved_account_keys.is_reserved(&pending_reserved_keys[0]));
        assert!(!reserved_account_keys.is_reserved(&pending_reserved_keys[1]));

        // Updating the active set with a default feature set should be a no-op
        let previous_reserved_account_keys = reserved_account_keys.clone();
        let mut feature_set = FeatureSet::default();
        reserved_account_keys.update_active_set(&feature_set);
        assert_eq!(reserved_account_keys, previous_reserved_account_keys);

        // Updating the active set with an activated feature should also activate
        // the corresponding reserved key from inactive to active
        feature_set.active.insert(feature_ids[0], 0);
        reserved_account_keys.update_active_set(&feature_set);

        assert!(reserved_account_keys.is_reserved(&active_reserved_key));
        assert!(reserved_account_keys.is_reserved(&pending_reserved_keys[0]));
        assert!(!reserved_account_keys.is_reserved(&pending_reserved_keys[1]));

        // Update the active set again to ensure that the inactive map is
        // properly retained
        feature_set.active.insert(feature_ids[1], 0);
        reserved_account_keys.update_active_set(&feature_set);

        assert!(reserved_account_keys.is_reserved(&active_reserved_key));
        assert!(reserved_account_keys.is_reserved(&pending_reserved_keys[0]));
        assert!(reserved_account_keys.is_reserved(&pending_reserved_keys[1]));
    }

    #[test]
    fn test_static_list_compat() {
        let mut static_set = HashSet::new();
        static_set.extend(ALL_IDS.iter().cloned());
        static_set.extend(BUILTIN_PROGRAMS_KEYS.iter().cloned());

        let initial_active_set = ReservedAccountKeys::default().active;

        assert_eq!(initial_active_set, static_set);
    }
}
