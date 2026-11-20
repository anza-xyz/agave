//! Configuration TOML parsing, version checks, and default merging.

use {
    crate::{EffectiveConfig, INPUT_DESCRIPTION, ValidatedConfig, validate_document},
    serde::{Deserialize, Serialize},
};

/// The built-in defaults and reference for the config file format.
pub const DEFAULT_CONFIG: &str = include_str!("../default_config.toml");

const SCHEMA_VERSION: i64 = 1;

/// The editable TOML values from a validator config, including its schema version.
///
/// Parsing and serialization preserve which fields were supplied and their values.
/// Serialization regenerates the TOML layout. Use [`Self::validate`] to check
/// the schema version, apply built-in defaults, and validate the effective settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ConfigDocument {
    table: toml::Table,
}

impl ConfigDocument {
    /// Parse the supplied TOML into an editable document.
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_with_description(text, INPUT_DESCRIPTION)
    }

    pub(crate) fn parse_with_description(text: &str, description: &str) -> Result<Self, String> {
        toml::from_str(text)
            .map_err(|error| format!("invalid config file `{description}`: {error}"))
    }

    /// Return the declared schema version if it is an integer.
    pub fn schema_version(&self) -> Option<i64> {
        self.table
            .get("schema_version")
            .and_then(toml::Value::as_integer)
    }

    /// The TOML fields supplied in this document.
    pub fn as_table(&self) -> &toml::Table {
        &self.table
    }

    /// Edit the TOML fields in this document.
    pub fn as_table_mut(&mut self) -> &mut toml::Table {
        &mut self.table
    }

    /// Serialize this document to TOML.
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|error| error.to_string())
    }

    /// Merge with the built-in defaults and validate the effective settings.
    pub fn validate(&self) -> Result<(ValidatedConfig, Vec<String>), String> {
        validate_document(Some(self.clone()), INPUT_DESCRIPTION)
    }

    fn into_overrides(mut self, description: &str) -> Result<toml::Table, String> {
        let version = self.schema_version().ok_or_else(|| {
            format!(
                "invalid config file `{description}`: schema_version must be set to an integer; \
                 this binary supports version {SCHEMA_VERSION}"
            )
        })?;
        if version != SCHEMA_VERSION {
            return Err(format!(
                "invalid config file `{description}`: schema_version {version} is unsupported; \
                 this binary supports version {SCHEMA_VERSION}"
            ));
        }
        self.table.remove("schema_version");
        Ok(self.table)
    }
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

/// Merge the user document into the built-in defaults.
pub(crate) fn merge_config(
    user: Option<ConfigDocument>,
    description: &str,
) -> Result<EffectiveConfig, String> {
    let mut merged = ConfigDocument::parse_with_description(DEFAULT_CONFIG, "<built-in>")?
        .into_overrides("<built-in>")?;
    if let Some(user) = user {
        merge_tables(
            &mut merged,
            user.into_overrides(description)?,
            &mut Vec::new(),
        );
    }
    merged
        .try_into()
        .map_err(|error| format!("invalid config file `{description}`: {error}"))
}
