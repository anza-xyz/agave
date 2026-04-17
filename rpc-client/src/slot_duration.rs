use {
    solana_account::{Account, ReadableAccount},
    solana_clock::{
        DEFAULT_MS_PER_SLOT, MAX_PROCESSING_AGE, MAX_RECENT_BLOCKHASHES,
        NUM_CONSECUTIVE_LEADER_SLOTS,
    },
    solana_pubkey::Pubkey,
    std::{sync::LazyLock, time::Duration},
};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BankTimingConfig {
    pub ns_per_slot: u128,
    pub max_processing_age: usize,
    pub max_recent_blockhashes: usize,
}

impl Default for BankTimingConfig {
    fn default() -> Self {
        Self {
            ns_per_slot: u128::from(DEFAULT_MS_PER_SLOT) * 1_000_000,
            max_processing_age: MAX_PROCESSING_AGE,
            max_recent_blockhashes: MAX_RECENT_BLOCKHASHES,
        }
    }
}

impl BankTimingConfig {
    pub fn slot_duration(self) -> Duration {
        Duration::from_nanos(u64::try_from(self.ns_per_slot).unwrap_or(u64::MAX))
    }

    pub fn half_slot_duration(self) -> Duration {
        let half_slot = self.slot_duration().checked_div(2).unwrap_or_default();
        if half_slot.is_zero() {
            Duration::from_nanos(1)
        } else {
            half_slot
        }
    }

    pub fn duration_for_slots(self, slots: u64) -> Duration {
        let slot_nanos = self.ns_per_slot.saturating_mul(u128::from(slots));
        Duration::from_nanos(u64::try_from(slot_nanos).unwrap_or(u64::MAX))
    }

    pub fn leader_window_duration(self) -> Duration {
        self.duration_for_slots(NUM_CONSECUTIVE_LEADER_SLOTS)
    }

    pub fn max_processing_age_duration(self) -> Duration {
        self.duration_for_slots(self.max_processing_age as u64)
    }

    pub fn max_recent_blockhashes_duration(self) -> Duration {
        self.duration_for_slots(self.max_recent_blockhashes as u64)
    }
}

static NS_PER_SLOT_ACCOUNT: LazyLock<Pubkey> = LazyLock::new(|| {
    let (pubkey, _) =
        Pubkey::find_program_address(&[b"nsperslot"], &agave_feature_set::alpenglow::id());
    pubkey
});

static MAX_PROCESSING_AGE_ACCOUNT: LazyLock<Pubkey> = LazyLock::new(|| {
    let (pubkey, _) = Pubkey::find_program_address(
        &[b"maxprocessingage"],
        &agave_feature_set::alpenglow::id(),
    );
    pubkey
});

static MAX_RECENT_BLOCKHASHES_ACCOUNT: LazyLock<Pubkey> = LazyLock::new(|| {
    let (pubkey, _) = Pubkey::find_program_address(
        &[b"maxrecentblockhashes"],
        &agave_feature_set::alpenglow::id(),
    );
    pubkey
});

pub fn ns_per_slot_account() -> Pubkey {
    *NS_PER_SLOT_ACCOUNT
}

pub fn max_processing_age_account() -> Pubkey {
    *MAX_PROCESSING_AGE_ACCOUNT
}

pub fn max_recent_blockhashes_account() -> Pubkey {
    *MAX_RECENT_BLOCKHASHES_ACCOUNT
}

pub fn bank_timing_accounts() -> [Pubkey; 3] {
    [
        ns_per_slot_account(),
        max_processing_age_account(),
        max_recent_blockhashes_account(),
    ]
}

pub fn bank_timing_config_from_accounts(accounts: &[Option<Account>]) -> Result<BankTimingConfig, String> {
    if accounts.len() != 3 {
        return Err(format!(
            "expected 3 bank timing accounts, received {}",
            accounts.len()
        ));
    }

    let defaults = BankTimingConfig::default();
    let ns_per_slot = decode_optional_u128_account(
        accounts[0].as_ref(),
        "ns_per_slot",
        defaults.ns_per_slot,
    )?;
    let max_processing_age = decode_optional_u64_account(
        accounts[1].as_ref(),
        "max_processing_age",
        defaults.max_processing_age as u64,
    )?;
    let max_recent_blockhashes = decode_optional_u64_account(
        accounts[2].as_ref(),
        "max_recent_blockhashes",
        defaults.max_recent_blockhashes as u64,
    )?;

    Ok(BankTimingConfig {
        ns_per_slot,
        max_processing_age: usize::try_from(max_processing_age)
            .map_err(|_| "max_processing_age does not fit in usize".to_string())?,
        max_recent_blockhashes: usize::try_from(max_recent_blockhashes)
            .map_err(|_| "max_recent_blockhashes does not fit in usize".to_string())?,
    })
}

fn decode_optional_u64_account(
    account: Option<&Account>,
    label: &str,
    default: u64,
) -> Result<u64, String> {
    decode_optional_fixed_width_account(account, label, default, |bytes| {
        u64::from_le_bytes(bytes)
    })
}

fn decode_optional_u128_account(
    account: Option<&Account>,
    label: &str,
    default: u128,
) -> Result<u128, String> {
    decode_optional_fixed_width_account(account, label, default, |bytes| {
        u128::from_le_bytes(bytes)
    })
}

fn decode_optional_fixed_width_account<T, const N: usize>(
    account: Option<&Account>,
    label: &str,
    default: T,
    decode: impl FnOnce([u8; N]) -> T,
) -> Result<T, String> {
    let Some(account) = account else {
        return Ok(default);
    };
    if account.data().is_empty() {
        return Ok(default);
    }
    let data: [u8; N] = account.data().try_into().map_err(|_| {
        format!(
            "{label} account has invalid size {}, expected {N}",
            account.data().len()
        )
    })?;
    Ok(decode(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_with_data(data: &[u8]) -> Option<Account> {
        Some(Account {
            lamports: 1,
            data: data.to_vec(),
            owner: Pubkey::default(),
            executable: false,
            rent_epoch: 0,
        })
    }

    #[test]
    fn test_bank_timing_config_from_accounts_uses_defaults_when_accounts_are_missing() {
        assert_eq!(
            bank_timing_config_from_accounts(&[None, None, None]).unwrap(),
            BankTimingConfig::default(),
        );
    }

    #[test]
    fn test_bank_timing_config_from_accounts_decodes_values() {
        let config = bank_timing_config_from_accounts(&[
            account_with_data(&123u128.to_le_bytes()),
            account_with_data(&456u64.to_le_bytes()),
            account_with_data(&789u64.to_le_bytes()),
        ])
        .unwrap();

        assert_eq!(
            config,
            BankTimingConfig {
                ns_per_slot: 123,
                max_processing_age: 456,
                max_recent_blockhashes: 789,
            }
        );
    }
}
