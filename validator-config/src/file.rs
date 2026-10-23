//! Configuration file loading, version checks, and default merging.

use {crate::EffectiveConfig, std::path::Path};

/// The built-in defaults and reference for the config file format.
pub const DEFAULT_CONFIG: &str = include_str!("../default_config.toml");

const SCHEMA_VERSION: i64 = 1;

fn parse_toml(text: &str, description: &str) -> Result<toml::Table, String> {
    let mut table: toml::Table = toml::from_str(text)
        .map_err(|error| format!("invalid config file `{description}`: {error}"))?;
    let version = table
        .remove("schema_version")
        .and_then(|version| version.as_integer())
        .ok_or_else(|| {
            format!(
                "invalid config file `{description}`: schema_version must be set to an integer; \
                 this binary supports version {SCHEMA_VERSION}"
            )
        })?;
    if version != SCHEMA_VERSION {
        return Err(format!(
            "invalid config file `{description}`: schema_version {version} is unsupported; this \
             binary supports version {SCHEMA_VERSION}"
        ));
    }
    Ok(table)
}

fn merge_tables(base: &mut toml::Table, user: toml::Table, path: &mut Vec<String>) {
    // The user supplies the interface labels; matching labels inherit fields.
    if path.len() == 1 && path[0] == "interfaces" {
        base.retain(|label, _| user.contains_key(label));
    }
    for (key, value) in user {
        // Device and worker selectors must not combine variants across files.
        let replace = (path.len() == 2 && path[0] == "interfaces" && key == "device")
            || (path.len() == 3 && path[0] == "interfaces" && path[2] == "xdp" && key == "workers");
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base)), toml::Value::Table(user)) if !replace => {
                path.push(key);
                merge_tables(base, user, path);
                path.pop();
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Read the built-in defaults merged with the user config file.
pub(crate) fn read_config(
    user_path: Option<&Path>,
    description: &str,
) -> Result<EffectiveConfig, String> {
    let mut merged = parse_toml(DEFAULT_CONFIG, "<built-in>")?;
    if let Some(path) = user_path {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("failed to read config file `{description}`: {error}"))?;
        let user = parse_toml(&text, description)?;
        merge_tables(&mut merged, user, &mut Vec::new());
    }
    merged
        .try_into()
        .map_err(|error| format!("invalid config file `{description}`: {error}"))
}
