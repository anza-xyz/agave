use {agave_tpu_extension_api::AccountFilter, solana_pubkey::Pubkey, std::collections::HashSet};

const TIP_ACCOUNTS: [&str; 8] = [
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "4xDsmeTWPNjgSVSS1VTfzFq3iHZhp77ffPkAmkZkdu71",
];

pub fn tip_account_pubkeys() -> impl Iterator<Item = Pubkey> {
    TIP_ACCOUNTS.iter().map(|s| s.parse().unwrap())
}

/// Blocks packets that touch Jito tip accounts.
///
/// Tip accounts must not be written by regular transactions while bundles
/// are using them to collect MEV fees. `jito_mainnet` loads the eight
/// canonical mainnet tip account addresses.
pub struct TipAccountFilter(HashSet<Pubkey>);

impl TipAccountFilter {
    pub fn jito_mainnet() -> Self {
        Self(tip_account_pubkeys().collect())
    }
}

impl AccountFilter for TipAccountFilter {
    #[inline(always)]
    fn is_blocked(&self, pubkey: &Pubkey) -> bool {
        self.0.contains(pubkey)
    }

    #[inline(always)]
    fn is_active(&self) -> bool {
        !self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_accounts_are_blocked() {
        let filter = TipAccountFilter::jito_mainnet();
        for account in TIP_ACCOUNTS {
            assert!(filter.is_blocked(&account.parse().unwrap()));
        }
    }

    #[test]
    fn non_tip_account_passes() {
        assert!(!TipAccountFilter::jito_mainnet().is_blocked(&Pubkey::default()));
    }
}
