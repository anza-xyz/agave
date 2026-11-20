//! Configuration TOML parsing, version checks, and default merging.

use {
    crate::{Diagnostic, DiagnosticKind, FieldPathSegment, ValidatedConfig, xdp},
    std::{fmt, ops::Range, path::Path, sync::Arc},
    toml::{
        Spanned,
        de::{DeTable, DeValue},
    },
};

/// The built-in defaults and reference for the config file format.
pub const DEFAULT_CONFIG: &str = include_str!("../default_config.toml");

const SCHEMA_VERSION: i64 = 1;
const DEFAULT_DESCRIPTION: &str = "<built-in>";
const INPUT_DESCRIPTION: &str = "<input>";

/// The editable TOML values from a validator config, including its schema version.
///
/// Parsing and serialization preserve which fields were supplied and their values.
/// Serialization regenerates the TOML layout. Use [`Self::validate`] to check
/// the schema version, apply built-in defaults, and validate the effective settings.
#[derive(Clone)]
pub struct ConfigDocument {
    table: toml::Table,
    source_text: Option<Arc<str>>,
}

impl fmt::Debug for ConfigDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigDocument")
            .field("table", &self.table)
            .finish()
    }
}

impl ConfigDocument {
    /// Parse the supplied TOML into an editable document.
    pub fn parse(text: &str) -> Result<Self, Diagnostic> {
        Ok(Self {
            table: parse_table(text, INPUT_DESCRIPTION)?,
            source_text: Some(Arc::from(text)),
        })
    }

    /// Return the declared schema version if it is an integer.
    pub fn schema_version(&self) -> Option<i64> {
        schema_version(&self.table)
    }

    /// The TOML fields supplied in this document.
    pub fn as_table(&self) -> &toml::Table {
        &self.table
    }

    /// Edit the TOML fields in this document. This clears the source locations.
    pub fn as_table_mut(&mut self) -> &mut toml::Table {
        self.source_text = None;
        &mut self.table
    }

    /// Serialize this document to TOML.
    pub fn to_toml(&self) -> Result<String, Diagnostic> {
        toml::to_string_pretty(&self.table)
            .map_err(|error| Diagnostic::new(DiagnosticKind::Serialization, error.to_string()))
    }

    /// Merge with the built-in defaults and validate the effective settings.
    pub fn validate(&self) -> Result<(ValidatedConfig, Vec<Diagnostic>), Diagnostic> {
        validate_table(
            Some(self.table.clone()),
            INPUT_DESCRIPTION,
            self.source_text.as_deref(),
        )
    }
}

/// Load the built-in defaults merged with the user config file and validate the result.
pub fn load(user_path: Option<&Path>) -> Result<(ValidatedConfig, Vec<Diagnostic>), Diagnostic> {
    let Some(path) = user_path else {
        return validate_table(None, DEFAULT_DESCRIPTION, Some(DEFAULT_CONFIG));
    };
    let description = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|error| {
        Diagnostic::new(DiagnosticKind::Io, error.to_string()).in_source(&description)
    })?;
    let table = parse_table(&text, &description)?;
    validate_table(Some(table), &description, Some(&text))
}

/// Parse user TOML, merge it with the built-in defaults, and validate the result.
///
/// The input must include the supported `schema_version`. The returned list
/// contains any validation warnings.
///
/// ```
/// use agave_validator_config::parse;
///
/// let (config, warnings) = parse("schema_version = 1\nxdp.enabled = false")?;
/// assert!(!config.xdp_enabled());
/// assert!(warnings.is_empty());
/// # Ok::<(), agave_validator_config::Diagnostic>(())
/// ```
pub fn parse(text: &str) -> Result<(ValidatedConfig, Vec<Diagnostic>), Diagnostic> {
    let table = parse_table(text, INPUT_DESCRIPTION)?;
    validate_table(Some(table), INPUT_DESCRIPTION, Some(text))
}

fn parse_table(text: &str, description: &str) -> Result<toml::Table, Diagnostic> {
    toml::from_str(text).map_err(|error| Diagnostic::toml_syntax(error, description))
}

fn schema_version(table: &toml::Table) -> Option<i64> {
    table
        .get("schema_version")
        .and_then(toml::Value::as_integer)
}

fn remove_schema_version(table: &mut toml::Table) -> Result<(), Diagnostic> {
    let schema_error = |message| {
        Diagnostic::new(DiagnosticKind::SchemaVersion, message).at_field(["schema_version"])
    };
    let version = schema_version(table).ok_or_else(|| {
        schema_error(format!(
            "must be set to an integer; this binary supports version {SCHEMA_VERSION}"
        ))
    })?;
    if version != SCHEMA_VERSION {
        return Err(schema_error(format!(
            "{version} is unsupported; this binary supports version {SCHEMA_VERSION}"
        )));
    }
    table.remove("schema_version");
    Ok(())
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

fn validate_table(
    user: Option<toml::Table>,
    description: &str,
    source_text: Option<&str>,
) -> Result<(ValidatedConfig, Vec<Diagnostic>), Diagnostic> {
    let mut merged = parse_table(DEFAULT_CONFIG, DEFAULT_DESCRIPTION)?;
    remove_schema_version(&mut merged).map_err(|mut error| {
        attach_source_locations(
            std::slice::from_mut(&mut error),
            DEFAULT_DESCRIPTION,
            Some(DEFAULT_CONFIG),
        );
        error
    })?;
    let with_source = |mut error| {
        attach_source_locations(std::slice::from_mut(&mut error), description, source_text);
        error
    };
    if let Some(mut user) = user {
        remove_schema_version(&mut user).map_err(with_source)?;
        merge_tables(&mut merged, user, &mut Vec::new());
    }
    let (config, mut warnings) = serde_path_to_error::deserialize(merged)
        .map_err(Diagnostic::schema)
        .and_then(xdp::validate)
        .map_err(with_source)?;
    attach_source_locations(&mut warnings, description, source_text);
    Ok((config, warnings))
}

fn attach_source_locations(
    diagnostics: &mut [Diagnostic],
    description: &str,
    source_text: Option<&str>,
) {
    if diagnostics.is_empty() {
        return;
    }
    let source_table = source_text.and_then(|text| DeTable::parse(text).ok());
    for diagnostic in diagnostics {
        diagnostic.source_name = Some(description.to_owned());
        diagnostic.source_span = source_table
            .as_ref()
            .and_then(|table| find_span(table.get_ref(), diagnostic.field_path()));
    }
}

fn find_span(table: &DeTable<'_>, path: &[FieldPathSegment]) -> Option<Range<usize>> {
    let (FieldPathSegment::Key(first), rest) = path.split_first()? else {
        return None;
    };
    let mut value = table_value(table, first)?;
    for segment in rest {
        value = match segment {
            FieldPathSegment::Key(key) => table_value(value.get_ref().as_table()?, key)?,
            FieldPathSegment::Index(index) => value.get_ref().as_array()?.get(*index)?,
        };
    }
    Some(value.span())
}

fn table_value<'a, 'i>(table: &'a DeTable<'i>, key: &str) -> Option<&'a Spanned<DeValue<'i>>> {
    table
        .iter()
        .find_map(|(name, value)| (name.get_ref() == key).then_some(value))
}
