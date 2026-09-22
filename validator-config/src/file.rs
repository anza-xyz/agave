//! Configuration file loading, version checks, and default merging.

use {
    crate::{EffectiveConfig, SCHEMA_VERSION, Source, validate_schema_version},
    std::path::Path,
};

/// The embedded default policy, as shipped, for `--print-default-config`.
pub const DEFAULT_CONFIG: &str = include_str!("../default_config.toml");

fn parse_toml(text: &str, description: &str) -> Result<toml::Value, String> {
    let value: toml::Value = toml::from_str(text)
        .map_err(|error| format!("invalid config file `{description}`: {error}"))?;
    let version = match value.get("schema_version") {
        Some(value) => value.as_integer().ok_or_else(|| {
            format!(
                "config `{description}` has a non-integer schema_version; this binary supports \
                 version {SCHEMA_VERSION}"
            )
        })?,
        None => {
            return Err(format!(
                "config `{description}` is missing required schema_version; this binary supports \
                 version {SCHEMA_VERSION}"
            ));
        }
    };
    validate_schema_version(version).map_err(|error| format!("config `{description}`: {error}"))?;
    Ok(value)
}

fn merge_value(base: &mut toml::Value, user: toml::Value, path: &mut Vec<String>) {
    let atomic = (path.len() == 3 && path[0] == "interfaces" && path[2] == "device")
        || (path.len() == 4 && path[0] == "interfaces" && path[2] == "xdp" && path[3] == "workers");
    if atomic {
        *base = user;
        return;
    }
    match (base, user) {
        (toml::Value::Table(base), toml::Value::Table(user)) => {
            if path.len() == 1 && path[0] == "interfaces" {
                base.retain(|label, _| user.contains_key(label));
            }
            for (key, value) in user {
                match base.get_mut(&key) {
                    Some(base) => {
                        path.push(key);
                        merge_value(base, value, path);
                        path.pop();
                    }
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, user) => *base = user,
    }
}

fn mark_user_sources(config: &mut EffectiveConfig, user: &toml::Value) {
    if let Some(interfaces) = user.get("interfaces").and_then(toml::Value::as_table) {
        for (label, interface) in &mut config.interfaces {
            let Some(interface_patch) = interfaces.get(label).and_then(toml::Value::as_table)
            else {
                continue;
            };
            if interface_patch.contains_key("device") {
                interface.device_source = Source::User;
            }
            let Some(xdp) = interface_patch.get("xdp").and_then(toml::Value::as_table) else {
                continue;
            };
            if xdp.contains_key("zero_copy") {
                interface.xdp.zero_copy_source = Source::User;
            }
            if xdp.contains_key("workers") {
                interface.xdp.workers_source = Source::User;
            }
        }
    }
}

pub fn load(user_path: Option<&Path>) -> Result<EffectiveConfig, String> {
    let mut merged = parse_toml(DEFAULT_CONFIG, "<built-in>")?;
    let description = user_path.map_or_else(
        || "<built-in>".to_string(),
        |path| path.display().to_string(),
    );
    let user = if let Some(path) = user_path {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("failed to read config file `{description}`: {error}"))?;
        let user = parse_toml(&text, &description)?;
        merge_value(&mut merged, user.clone(), &mut Vec::new());
        Some(user)
    } else {
        None
    };
    let mut config: EffectiveConfig = merged
        .try_into()
        .map_err(|error| format!("invalid config file `{description}`: {error}"))?;
    config
        .validate_structural()
        .map_err(|error| format!("invalid config file `{description}`: {error}"))?;
    if let Some(user) = user {
        mark_user_sources(&mut config, &user);
    }
    Ok(config)
}
