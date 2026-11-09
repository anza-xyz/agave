use {
    crate::parse_account_data::{ParsableAccount, ParseAccountError},
    serde::{Deserialize, Serialize},
    serde_json::Value,
    solana_account::{WINCODE_CONFIG, WincodeConfig},
    solana_config_interface::state::{ConfigKeys, get_config_data},
    solana_pubkey::Pubkey,
};

const VALIDATOR_INFO_ID: Pubkey =
    solana_pubkey::pubkey!("Va1idator1nfo111111111111111111111111111111");

pub fn parse_config(data: &[u8], _pubkey: &Pubkey) -> Result<ConfigAccountType, ParseAccountError> {
    let parsed_account = wincode::config::deserialize::<ConfigKeys, _>(data, WINCODE_CONFIG)
        .ok()
        .and_then(|key_list| {
            if !key_list.keys.is_empty() && key_list.keys[0].0 == VALIDATOR_INFO_ID {
                parse_config_data::<String>(data, key_list.keys).and_then(|validator_info| {
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

fn parse_config_data<T>(data: &[u8], keys: Vec<(Pubkey, bool)>) -> Option<UiConfig<T>>
where
    T: for<'de> wincode::SchemaRead<'de, WincodeConfig, Dst = T>,
{
    let config_data: T =
        wincode::config::deserialize(get_config_data(data).ok()?, WINCODE_CONFIG).ok()?;
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
        serde_json::json,
        solana_account::{AccountSharedData, ReadableAccount, state_traits::StateMutWincode as _},
        wincode::{SchemaRead, SchemaWrite},
    };

    #[derive(SchemaRead, SchemaWrite)]
    struct ValidatorInfo {
        info: String,
    }

    fn create_config_account(
        keys: Vec<(Pubkey, bool)>,
        validator_info: ValidatorInfo,
        lamports: u64,
    ) -> AccountSharedData {
        AccountSharedData::new_data(
            lamports,
            &(ConfigKeys { keys }, validator_info),
            &solana_sdk_ids::config::id(),
        )
        .unwrap()
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
            vec![(VALIDATOR_INFO_ID, false), (info_pubkey, true)],
            validator_info,
            10,
        );
        assert_eq!(
            parse_config(validator_info_config_account.data(), &info_pubkey).unwrap(),
            ConfigAccountType::ValidatorInfo(UiConfig {
                keys: vec![
                    UiConfigKey {
                        pubkey: VALIDATOR_INFO_ID.to_string(),
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
}
