use {
    serde::{Deserialize, Serialize},
    std::path::PathBuf,
};

/// Precedence: CLI arguments > config file > defaults
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorConfig {
    pub ledger: Option<PathBuf>,
    pub identity: Option<String>,
    pub entrypoint: Option<Vec<String>>,
    pub known_validators: Option<Vec<String>>,
    pub rpc_port: Option<u16>,
    pub log: Option<String>,
}

impl ValidatorConfig {
    pub fn load_from_file(path: &str) -> Result<Self, String> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file {}: {}", path, e))?;
        toml::from_str::<Self>(&contents)
            .map_err(|e| format!("Invalid TOML in config file {}: {}", path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_parsing() {
        let toml_str = r#"
ledger = "/tmp/ledger"
identity = "/tmp/identity.json"
entrypoint = ["127.0.0.1:8001"]
known_validators = ["GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ"]
rpc_port = 8899
log = "/tmp/validator.log"
"#;
        let config: ValidatorConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.ledger, Some(PathBuf::from("/tmp/ledger")));
        assert_eq!(config.identity, Some("/tmp/identity.json".to_string()));
        assert_eq!(config.entrypoint.as_ref().unwrap().len(), 1);
        assert_eq!(config.known_validators.as_ref().unwrap().len(), 1);
        assert_eq!(config.rpc_port, Some(8899));
        assert_eq!(config.log.as_deref(), Some("/tmp/validator.log"));
    }

    #[test]
    fn test_unknown_fields_rejected() {
        let toml_str = r#"
ledger = "/tmp/ledger"
foo = "bar"
"#;
        let result: Result<ValidatorConfig, _> = toml::from_str(toml_str);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown field"));
    }

    #[test]
    fn test_parse_new_fields() {
        let toml_str = r#"
ledger = "/l"
identity = "/i.json"
entrypoint = ["127.0.0.1:8001"]
known_validators = ["7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2"]
rpc_port = 8899
log = "/v.log"
"#;
        let cfg: ValidatorConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.rpc_port, Some(8899));
        assert_eq!(cfg.log.as_deref(), Some("/v.log"));
        assert_eq!(cfg.known_validators.as_ref().unwrap().len(), 1);
    }
}
