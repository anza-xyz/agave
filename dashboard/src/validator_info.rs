//! Validator display names, read from on-chain `ValidatorInfo` config
//! accounts.
//!
//! The account addresses are not derived from the identity, so the only way to
//! find them is to search by owner, which is only affordable against the
//! secondary index (`--account-index program-id`). Without it the search is
//! skipped and the dashboard shows pubkeys. The cache is kept current
//! afterwards from each slot's own writes, which costs almost nothing.

use {
    serde::{Deserialize, Serialize},
    solana_account::ReadableAccount,
    solana_accounts_db::accounts_index::IndexKey,
    solana_config_interface::state::{ConfigKeys, get_config_data},
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::collections::HashMap,
};

/// Marker key present as the first entry of a `ValidatorInfo` config account.
const VALIDATOR_INFO_PROGRAM: Pubkey =
    Pubkey::from_str_const("Va1idator1nfo111111111111111111111111111111");

/// Upper bound on a valid `ValidatorInfo` account, used to skip oversized
/// config accounts without deserializing them.
const MAX_VALIDATOR_INFO_LEN: usize = 576 + 1 + (32 + 1) * 2;

/// The two fields the dashboard renders. The account also carries a website,
/// description and keybase name, which nothing displays.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ValidatorInfo {
    pub name: Option<String>,
    #[serde(rename = "iconUrl")]
    pub icon_url: Option<String>,
}

/// What every validator that published anything calls itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Displays {
    /// Base58 identities. `names[i]` and `icons[i]` belong to `keys[i]`.
    pub keys: Vec<String>,
    pub names: Vec<Option<String>>,
    pub icons: Vec<Option<String>>,
}

#[derive(Debug, Default)]
pub struct ValidatorInfoCache {
    by_identity: HashMap<Pubkey, ValidatorInfo>,
}

impl ValidatorInfoCache {
    pub fn get(&self, identity: &Pubkey) -> Option<&ValidatorInfo> {
        self.by_identity.get(identity)
    }

    /// Everything the cache holds, as three arrays sharing an index, since an
    /// object would carry the words name and icon once per validator. Separate
    /// from the epoch message because names change on their own schedule and most
    /// validators publish neither.
    pub fn displays(&self) -> Displays {
        let mut keys = Vec::with_capacity(self.by_identity.len());
        let mut names = Vec::with_capacity(self.by_identity.len());
        let mut icons = Vec::with_capacity(self.by_identity.len());
        for (identity, info) in &self.by_identity {
            if info.name.is_none() && info.icon_url.is_none() {
                continue;
            }
            keys.push(identity.to_string());
            names.push(info.name.clone());
            icons.push(info.icon_url.clone());
        }
        Displays { keys, names, icons }
    }

    pub fn len(&self) -> usize {
        self.by_identity.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_identity.is_empty()
    }

    /// Inserts an entry, returning true if it was new or changed.
    pub fn insert(&mut self, identity: Pubkey, info: ValidatorInfo) -> bool {
        if self.by_identity.get(&identity) == Some(&info) {
            return false;
        }
        self.by_identity.insert(identity, info);
        true
    }

    /// Merges the result of [`scan_all`], returning how many entries changed.
    /// Kept separate from the scan so the lock is only held for the merge.
    pub fn merge(&mut self, entries: Vec<(Pubkey, ValidatorInfo)>) -> usize {
        entries
            .into_iter()
            .filter(|(identity, info)| self.insert(*identity, info.clone()))
            .count()
    }
}

/// Validator info written in `bank`'s own slot, which works on every
/// validator, indexed or not. Returns rather than merges so a caller sweeping
/// several banks takes the cache lock once, and only if anything turned up.
pub fn scan_slot(bank: &Bank) -> Vec<(Pubkey, ValidatorInfo)> {
    bank.get_program_accounts_modified_since_parent(&solana_sdk_ids::config::id())
        .into_iter()
        .filter_map(|(_pubkey, account)| parse(account.data()))
        .collect()
}

/// Walks every config account and returns the validator info it finds. Minutes
/// on a real cluster: run on a background thread with no lock held across it.
pub fn scan_all(bank: &Bank) -> Vec<(Pubkey, ValidatorInfo)> {
    let config_id = solana_sdk_ids::config::id();

    // Refused when the config program is excluded from the index, because the
    // indexed call falls back to reading every account on the validator, which
    // takes hours and cannot be interrupted.
    if !bank.account_indexes_include_key(&config_id) {
        log::info!(
            "dashboard: the config program is excluded from the account index, so validator              names are unavailable and the dashboard will show pubkeys"
        );
        return Vec::new();
    }

    let accounts = match bank.get_filtered_indexed_accounts(
        &IndexKey::ProgramId(config_id),
        |account| account.owner() == &config_id,
        None,
    ) {
        Ok(accounts) => accounts,
        Err(err) => {
            log::warn!("dashboard: could not read validator info accounts: {err}");
            return Vec::new();
        }
    };

    // An empty result on a live cluster means the index is not switched on: an
    // index never built answers none rather than failing.
    if accounts.is_empty() {
        log::info!(
            "dashboard: found no validator info accounts. Start the validator with              --account-index program-id --account-index-include-key {config_id} to show              validator names instead of pubkeys"
        );
    }

    accounts
        .into_iter()
        .filter_map(|(_pubkey, account)| parse(account.data()))
        .collect()
}

/// The identity and its advertised info from a config account's raw data, or
/// `None` for other config accounts and malformed data.
fn parse(data: &[u8]) -> Option<(Pubkey, ValidatorInfo)> {
    if data.len() > MAX_VALIDATOR_INFO_LEN {
        return None;
    }
    let keys = bincode::deserialize::<ConfigKeys>(data).ok()?;
    // The first key marks the account type; the second is the identity that
    // signed for it, which is what we key the cache on.
    if keys.keys.first()?.0 != VALIDATOR_INFO_PROGRAM {
        return None;
    }
    let identity = keys.keys.get(1)?.0;

    // The config payload is a bincode string whose contents are JSON.
    let json = bincode::deserialize::<String>(get_config_data(data).ok()?).ok()?;
    let info = serde_json::from_str::<ValidatorInfo>(&json).ok()?;
    Some((identity, info))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::fixture::fixture,
        solana_account::{Account, AccountSharedData},
    };

    fn encode(keys: Vec<(Pubkey, bool)>, json: &str) -> Vec<u8> {
        let mut data = bincode::serialize(&ConfigKeys { keys }).unwrap();
        data.extend(bincode::serialize(&json.to_string()).unwrap());
        data
    }

    /// A validator-info account as it sits on chain, owned by the config
    /// program and signed for by `identity`.
    fn info_account(identity: Pubkey, json: &str) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports: 1,
            data: encode(
                vec![(VALIDATOR_INFO_PROGRAM, false), (identity, true)],
                json,
            ),
            owner: solana_sdk_ids::config::id(),
            executable: false,
            rent_epoch: 0,
        })
    }

    #[test]
    fn test_the_slot_sweep_finds_info_written_in_that_slot() {
        // The cheap path, run every few seconds against each newly frozen bank.
        let harness = fixture();
        let identity = Pubkey::new_unique();
        let bank = harness.advance_with(
            1,
            &[(
                Pubkey::new_unique(),
                info_account(identity, r#"{"name":"Lantern"}"#),
            )],
        );

        let found = scan_slot(&bank);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, identity);
        assert_eq!(found[0].1.name.as_deref(), Some("Lantern"));
    }

    #[test]
    fn test_the_slot_sweep_ignores_slots_that_wrote_nothing() {
        // Almost every slot: config accounts are written perhaps once a day across
        // the cluster.
        let harness = fixture();
        harness.advance_with(
            1,
            &[(
                Pubkey::new_unique(),
                info_account(Pubkey::new_unique(), r#"{"name":"Lantern"}"#),
            )],
        );
        let later = harness.advance_to(2);

        assert!(
            scan_slot(&later).is_empty(),
            "a slot's sweep must cover its own writes only"
        );
    }

    #[test]
    fn test_the_full_scan_finds_info_from_an_earlier_slot() {
        // The one-shot startup read that populates the cache. The fixture's bank
        // carries the config program in its index, without which this finds nothing
        // by design.
        let harness = fixture();
        let identity = Pubkey::new_unique();
        harness.advance_with(
            1,
            &[(
                Pubkey::new_unique(),
                info_account(identity, r#"{"name":"Lantern"}"#),
            )],
        );
        let later = harness.advance_to(2);

        let found = scan_all(&later);
        assert_eq!(found.len(), 1, "the scan should reach back past this slot");
        assert_eq!(found[0].0, identity);
    }

    #[test]
    fn test_merging_reports_only_what_changed() {
        // The count drives a debug line, but the filter behind it is what stops
        // an unchanged name republishing every slot that mentions it.
        let mut cache = ValidatorInfoCache::default();
        assert!(cache.is_empty());

        let identity = Pubkey::new_unique();
        let info = ValidatorInfo {
            name: Some("Lantern".into()),
            icon_url: None,
        };
        assert_eq!(cache.merge(vec![(identity, info.clone())]), 1);
        assert_eq!(cache.merge(vec![(identity, info)]), 0, "nothing changed");
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.get(&identity).and_then(|info| info.name.as_deref()),
            Some("Lantern")
        );
    }

    #[test]
    fn test_parses_a_validator_info_account() {
        let identity = Pubkey::new_unique();
        let data = encode(
            vec![(VALIDATOR_INFO_PROGRAM, false), (identity, true)],
            r#"{"name":"Lantern","iconUrl":"https://example.com/i.png"}"#,
        );
        let (parsed_identity, info) = parse(&data).unwrap();
        assert_eq!(parsed_identity, identity);
        assert_eq!(info.name.as_deref(), Some("Lantern"));
        assert_eq!(info.icon_url.as_deref(), Some("https://example.com/i.png"));
    }

    #[test]
    fn test_fields_the_dashboard_does_not_render_are_ignored() {
        // An unknown field must be skipped, or a validator publishing a website would
        // lose its name too.
        let identity = Pubkey::new_unique();
        let data = encode(
            vec![(VALIDATOR_INFO_PROGRAM, false), (identity, true)],
            r#"{"name":"Lantern","website":"https://example.com","details":"hi",
                "keybaseUsername":"lantern"}"#,
        );
        let (_, info) = parse(&data).unwrap();
        assert_eq!(info.name.as_deref(), Some("Lantern"));
    }

    #[test]
    fn test_ignores_other_config_accounts() {
        let data = encode(
            vec![(Pubkey::new_unique(), false), (Pubkey::new_unique(), true)],
            r#"{"name":"nope"}"#,
        );
        assert!(parse(&data).is_none());
    }

    #[test]
    fn test_ignores_malformed_data() {
        assert!(parse(&[]).is_none());
        assert!(parse(&[0xff; 32]).is_none());
        assert!(parse(&vec![0u8; MAX_VALIDATOR_INFO_LEN.saturating_add(1)]).is_none());
    }

    #[test]
    fn test_insert_reports_only_changes() {
        let mut cache = ValidatorInfoCache::default();
        let identity = Pubkey::new_unique();
        let info = ValidatorInfo {
            name: Some("Lantern".into()),
            ..ValidatorInfo::default()
        };
        assert!(cache.insert(identity, info.clone()));
        assert!(!cache.insert(identity, info));
        assert_eq!(cache.len(), 1);
    }
}
