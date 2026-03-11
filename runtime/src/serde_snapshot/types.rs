use {
    solana_accounts_db::accounts_hash::AccountsLtHash, solana_clock::Epoch,
    solana_epoch_schedule::EpochSchedule, solana_lattice_hash::lt_hash::LtHash, solana_rent::Rent,
};

/// Snapshot serde-safe AccountsLtHash
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[serde_with::serde_as]
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, PartialEq, Eq)]
pub struct SerdeAccountsLtHash(
    // serde only has array support up to 32 elements; anything larger needs to be handled manually
    // see https://github.com/serde-rs/serde/issues/1937 for more information
    #[serde_as(as = "[_; LtHash::NUM_ELEMENTS]")] pub [u16; LtHash::NUM_ELEMENTS],
);

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
#[cfg_attr(feature = "frozen-abi", derive(solana_frozen_abi_macro::AbiExample))]
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
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
