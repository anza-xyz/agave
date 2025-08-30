use std::error;

use bincode::deserialize;
use serde_json::Map;
use solana_account::Account;
use solana_config_interface::state::{get_config_data, ConfigKeys};
use solana_pubkey::Pubkey;

pub fn parse_validator_info(
    pubkey: &Pubkey,
    account: &Account,
) -> Result<(Pubkey, Map<String, serde_json::value::Value>), Box<dyn error::Error>> {
    if account.owner != solana_config_interface::id() {
        return Err(format!("{pubkey} is not a validator info account").into());
    }
    let key_list: ConfigKeys = deserialize(&account.data)?;
    if !key_list.keys.is_empty() {
        let (validator_pubkey, _) = key_list.keys[1];
        let validator_info_string: String = deserialize(get_config_data(&account.data)?)?;
        let validator_info: Map<_, _> = serde_json::from_str(&validator_info_string)?;
        Ok((validator_pubkey, validator_info))
    } else {
        Err(format!("{pubkey} could not be parsed as a validator info account").into())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::validator_info::{self, ValidatorInfo, MAX_VALIDATOR_INFO},
        bincode::{serialize, serialized_size},
        serde_json::Value,
    };

    #[test]
    fn test_parse_validator_info() {
        let pubkey = solana_pubkey::new_rand();
        let keys = vec![(validator_info::id(), false), (pubkey, true)];
        let config = ConfigKeys { keys };

        let mut info = Map::new();
        info.insert("name".to_string(), Value::String("Alice".to_string()));
        let info_string = serde_json::to_string(&Value::Object(info.clone())).unwrap();
        let validator_info = ValidatorInfo { info: info_string };
        let data = serialize(&(config, validator_info)).unwrap();

        assert_eq!(
            parse_validator_info(
                &Pubkey::default(),
                &Account {
                    owner: solana_config_interface::id(),
                    data,
                    ..Account::default()
                }
            )
            .unwrap(),
            (pubkey, info)
        );
    }

    #[test]
    fn test_parse_validator_info_not_validator_info_account() {
        assert!(parse_validator_info(
            &Pubkey::default(),
            &Account {
                owner: solana_pubkey::new_rand(),
                ..Account::default()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("is not a validator info account"));
    }

    #[test]
    fn test_parse_validator_info_empty_key_list() {
        let config = ConfigKeys { keys: vec![] };
        let validator_info = ValidatorInfo {
            info: String::new(),
        };
        let data = serialize(&(config, validator_info)).unwrap();

        assert!(parse_validator_info(
            &Pubkey::default(),
            &Account {
                owner: solana_config_interface::id(),
                data,
                ..Account::default()
            },
        )
        .unwrap_err()
        .to_string()
        .contains("could not be parsed as a validator info account"));
    }

    #[test]
    fn test_validator_info_max_space() {
        // 70-character string
        let max_short_string =
            "Max Length String KWpP299aFCBWvWg1MHpSuaoTsud7cv8zMJsh99aAtP8X1s26yrR1".to_string();
        // 300-character string
        let max_long_string = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Ut libero \
                               quam, volutpat et aliquet eu, varius in mi. Aenean vestibulum ex \
                               in tristique faucibus. Maecenas in imperdiet turpis. Nullam \
                               feugiat aliquet erat. Morbi malesuada turpis sed dui pulvinar \
                               lobortis. Pellentesque a lectus eu leo nullam."
            .to_string();
        let mut info = Map::new();
        info.insert("name".to_string(), Value::String(max_short_string.clone()));
        info.insert(
            "website".to_string(),
            Value::String(max_short_string.clone()),
        );
        info.insert(
            "keybaseUsername".to_string(),
            Value::String(max_short_string),
        );
        info.insert("details".to_string(), Value::String(max_long_string));
        let info_string = serde_json::to_string(&Value::Object(info)).unwrap();

        let validator_info = ValidatorInfo { info: info_string };

        assert_eq!(
            serialized_size(&validator_info).unwrap(),
            MAX_VALIDATOR_INFO
        );
    }
}
