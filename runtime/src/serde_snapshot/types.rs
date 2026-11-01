#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample};
#[cfg(not(feature = "stable-abi"))]
use wincode::{adapter::DiscardSeq, len::BincodeLen};
use {
    solana_account::AccountSharedData,
    solana_accounts_db::accounts_hash::AccountsLtHash,
    solana_clock::Epoch,
    solana_epoch_schedule::EpochSchedule,
    solana_lattice_hash::lt_hash::LtHash,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_stake_history::StakeHistoryItem,
    solana_stake_interface::state::Delegation,
    wincode::{SchemaRead, SchemaWrite},
};

/// Snapshot serde-safe AccountsLtHash
#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct SerdeAccountsLtHash(pub [u16; LtHash::NUM_ELEMENTS]);

impl From<SerdeAccountsLtHash> for AccountsLtHash {
    fn from(accounts_lt_hash: SerdeAccountsLtHash) -> Self {
        Self(LtHash(accounts_lt_hash.0))
    }
}
impl From<AccountsLtHash> for SerdeAccountsLtHash {
    fn from(accounts_lt_hash: AccountsLtHash) -> Self {
        Self(accounts_lt_hash.0.0)
    }
}

/// Snapshot serde-safe RentCollector, which is now unused
#[repr(C)]
#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Debug, Clone, SchemaRead, SchemaWrite)]
pub struct UnusedRentCollector {
    epoch: Epoch,
    epoch_schedule: EpochSchedule,
    slots_per_year: f64,
    rent: Rent,
}

impl UnusedRentCollector {
    pub fn zeroed() -> Self {
        #[allow(deprecated)]
        Self {
            epoch: 0,
            epoch_schedule: EpochSchedule {
                slots_per_epoch: 0,
                leader_schedule_slot_offset: 0,
                warmup: false,
                first_normal_epoch: 0,
                first_normal_slot: 0,
            },
            slots_per_year: 0.0,
            rent: Rent {
                lamports_per_byte: 0,
                exemption_threshold: [0; 8],
                burn_percent: 0,
            },
        }
    }
}

/// Snapshot serde-safe Stakes<Delegation>, which is now unused
#[repr(C)]
#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Debug, Clone, SchemaRead, SchemaWrite)]
pub struct UnusedStakes {
    #[cfg_attr(
        not(feature = "stable-abi"),
        wincode(with = "DiscardSeq<(Pubkey, (u64, AccountSharedData)), BincodeLen>")
    )]
    vote_accounts: Vec<(Pubkey, (u64, AccountSharedData))>,
    #[cfg_attr(
        not(feature = "stable-abi"),
        wincode(with = "DiscardSeq<(Pubkey, Delegation), BincodeLen>")
    )]
    stake_delegations: Vec<(Pubkey, Delegation)>,
    unused: u64,
    epoch: Epoch,
    stake_history: Vec<StakeHistoryItem>,
}

impl UnusedStakes {
    pub fn zeroed() -> Self {
        Self {
            vote_accounts: Vec::new(),
            stake_delegations: Vec::new(),
            unused: 0,
            epoch: 0,
            stake_history: Vec::new(),
        }
    }
}
