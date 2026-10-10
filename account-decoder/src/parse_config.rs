use {
    crate::{
        parse_account_data::{ParsableAccount, ParseAccountError},
        validator_info,
    },
    serde::{Deserialize, Serialize},
    serde_json::Value,
    solana_config_interface::state::{ConfigKeys, get_config_data},
    solana_pubkey::Pubkey,
};

const ACCOUNT_DATA_PREALLOCATION_LIMIT: usize = 10 * 1024 * 1024;

const ACCOUNT_DATA_WINCODE_CONFIG: wincode::config::Configuration<
    true,
    ACCOUNT_DATA_PREALLOCATION_LIMIT,
> = wincode::config::Configuration::default()
    .with_preallocation_size_limit::<ACCOUNT_DATA_PREALLOCATION_LIMIT>();

pub fn parse_config(data: &[u8], _pubkey: &Pubkey) -> Result<ConfigAccountType, ParseAccountError> {
    let parsed_account = wincode::deserialize::<ConfigKeys>(data)
        .ok()
        .and_then(|key_list| {
            if !key_list.keys.is_empty() && key_list.keys[0].0 == validator_info::id() {
                parse_config_data(data, key_list.keys).and_then(|validator_info| {
                    Some(ConfigAccountType::ValidatorInfo(UiConfig {
                        keys: validator_info.keys,
                        config_data: serde_json::from_str(&validator_info.config_data).ok()?,
                    }))
                })
            } else {
                None
            }
        });
    parsed_account.ok_or(ParseAccountError::AccountNotParsable(
        ParsableAccount::Config,
    ))
}

fn parse_config_data(data: &[u8], keys: Vec<(Pubkey, bool)>) -> Option<UiConfig<String>> {
    let config_data: String =
        wincode::config::deserialize(get_config_data(data).ok()?, ACCOUNT_DATA_WINCODE_CONFIG)
            .ok()?;

    let keys = keys
        .iter()
        .map(|key| UiConfigKey {
            pubkey: key.0.to_string(),
            signer: key.1,
        })
        .collect();

    Some(UiConfig { keys, config_data })
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "type", content = "info")]
pub enum ConfigAccountType {
    ValidatorInfo(UiConfig<Value>),
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UiConfigKey {
    pub pubkey: String,
    pub signer: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UiConfig<T> {
    pub keys: Vec<UiConfigKey>,
    pub config_data: T,
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::validator_info::ValidatorInfo,
        serde_json::json,
        solana_account::{Account, AccountSharedData, ReadableAccount},
    };

    fn create_config_account<T: wincode::Serialize<Src = T>>(
        keys: Vec<(Pubkey, bool)>,
        config_data: &T,
        lamports: u64,
    ) -> AccountSharedData {
        let mut data = wincode::serialize(&ConfigKeys { keys }).unwrap();
        data.extend_from_slice(&wincode::serialize(config_data).unwrap());
        AccountSharedData::from(Account {
            lamports,
            data,
            owner: solana_sdk_ids::config::id(),
            ..Account::default()
        })
    }

    #[test]
    fn test_parse_config() {
        let validator_info = ValidatorInfo {
            info: serde_json::to_string(&json!({
                "name": "Solana",
            }))
            .unwrap(),
        };
        let info_pubkey = solana_pubkey::new_rand();
        let validator_info_config_account = create_config_account(
            vec![(validator_info::id(), false), (info_pubkey, true)],
            &validator_info,
            10,
        );
        assert_eq!(
            parse_config(validator_info_config_account.data(), &info_pubkey).unwrap(),
            ConfigAccountType::ValidatorInfo(UiConfig {
                keys: vec![
                    UiConfigKey {
                        pubkey: validator_info::id().to_string(),
                        signer: false,
                    },
                    UiConfigKey {
                        pubkey: info_pubkey.to_string(),
                        signer: true,
                    }
                ],
                config_data: serde_json::from_str(r#"{"name":"Solana"}"#).unwrap(),
            }),
        );

        let bad_data = vec![0; 4];
        assert!(parse_config(&bad_data, &info_pubkey).is_err());
    }

    #[test]
    fn test_parse_config_validator_info_larger_than_default_prealloc_limit() {
        let validator_info = ValidatorInfo {
            info: serde_json::to_string(&json!({
                "name": "a".repeat(5 * 1024 * 1024),
            }))
            .unwrap(),
        };

        let info_pubkey = solana_pubkey::new_rand();

        let mut data = wincode::serialize(&ConfigKeys {
            keys: vec![(validator_info::id(), false), (info_pubkey, true)],
        })
        .unwrap();

        // Construct the String payload manually because wincode's
        // default serializer also rejects strings larger than 4 MiB.
        let info_bytes = validator_info.info.as_bytes();

        data.extend_from_slice(&(info_bytes.len() as u64).to_le_bytes());
        data.extend_from_slice(info_bytes);

        let config_account = AccountSharedData::from(Account {
            lamports: 10,
            data,
            owner: solana_sdk_ids::config::id(),
            ..Account::default()
        });

        let parsed = parse_config(config_account.data(), &info_pubkey)
            .expect("Valid config account larger than 4 MiB should be parsed successfully");

        let ConfigAccountType::ValidatorInfo(parsed_info) = parsed;

        let expected_json: Value = serde_json::from_str(&validator_info.info).unwrap();

        assert_eq!(parsed_info.config_data, expected_json);
        assert_eq!(parsed_info.keys.len(), 2);
        assert_eq!(parsed_info.keys[0].pubkey, validator_info::id().to_string());
        assert_eq!(parsed_info.keys[1].pubkey, info_pubkey.to_string());
    }

    #[test]
    fn test_parse_config_validator_info_near_max_account_size() {
        let validator_info = ValidatorInfo {
            info: serde_json::to_string(&json!({
                "name": "a".repeat(9 * 1024 * 1024),
            }))
            .unwrap(),
        };

        let info_pubkey = solana_pubkey::new_rand();

        let mut data = wincode::serialize(&ConfigKeys {
            keys: vec![(validator_info::id(), false), (info_pubkey, true)],
        })
        .unwrap();

        let info_bytes = validator_info.info.as_bytes();
        data.extend_from_slice(&(info_bytes.len() as u64).to_le_bytes());
        data.extend_from_slice(info_bytes);

        assert!(data.len() <= 10 * 1024 * 1024);

        let parsed = parse_config(&data, &info_pubkey)
            .expect("Valid account near the maximum size should be parsed");

        let ConfigAccountType::ValidatorInfo(parsed_info) = parsed;

        let expected_json: Value = serde_json::from_str(&validator_info.info).unwrap();

        assert_eq!(parsed_info.config_data, expected_json);
    }

    #[test]
    fn test_parse_config_rejects_string_exceeding_prealloc_limit() {
        let validator_info = ValidatorInfo {
            info: serde_json::to_string(&json!({
                "name": "a".repeat(ACCOUNT_DATA_PREALLOCATION_LIMIT),
            }))
            .unwrap(),
        };

        let info_pubkey = solana_pubkey::new_rand();

        let mut data = wincode::serialize(&ConfigKeys {
            keys: vec![(validator_info::id(), false), (info_pubkey, true)],
        })
        .unwrap();

        let info_bytes = validator_info.info.as_bytes();

        assert!(info_bytes.len() > ACCOUNT_DATA_PREALLOCATION_LIMIT);
        assert!(serde_json::from_str::<Value>(&validator_info.info).is_ok());

        data.extend_from_slice(&(info_bytes.len() as u64).to_le_bytes());
        data.extend_from_slice(info_bytes);

        assert!(
            parse_config(&data, &info_pubkey).is_err(),
            "String exceeding the preallocation limit must be rejected"
        );
    }
}
