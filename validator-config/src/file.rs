//! Configuration TOML parsing, version checks, and default merging.

use {
    crate::{
        Diagnostic, DiagnosticKind, EffectiveConfig, FieldPathSegment, INPUT_DESCRIPTION,
        ValidatedConfig, validate_document,
    },
    std::{fmt, ops::Range, sync::Arc},
    toml::{
        Spanned,
        de::{DeTable, DeValue},
    },
};

/// The built-in defaults and reference for the config file format.
pub const DEFAULT_CONFIG: &str = include_str!("../default_config.toml");

const SCHEMA_VERSION: i64 = 1;

/// The editable TOML values from a validator config, including its schema version.
///
/// Parsing and serialization preserve which fields were supplied and their values.
/// Serialization regenerates the TOML layout. Use [`Self::validate`] to check
/// the schema version, apply built-in defaults, and validate the effective settings.
#[derive(Clone)]
pub struct ConfigDocument {
    table: toml::Table,
    pub(crate) source_text: Option<Arc<str>>,
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
        Self::parse_with_description(text, INPUT_DESCRIPTION)
    }

    pub(crate) fn parse_with_description(
        text: &str,
        description: &str,
    ) -> Result<Self, Diagnostic> {
        let table =
            toml::from_str(text).map_err(|error| Diagnostic::toml_syntax(error, description))?;
        Ok(Self {
            table,
            source_text: Some(Arc::from(text)),
        })
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
        validate_document(Some(self.clone()), INPUT_DESCRIPTION)
    }

    fn into_overrides(mut self, description: &str) -> Result<toml::Table, Diagnostic> {
        let schema_error = |message| {
            Diagnostic::new(DiagnosticKind::SchemaVersion, message)
                .at_field(["schema_version"])
                .in_source(description)
        };
        let version = self.schema_version().ok_or_else(|| {
            schema_error(format!(
                "must be set to an integer; this binary supports version {SCHEMA_VERSION}"
            ))
        })?;
        if version != SCHEMA_VERSION {
            return Err(schema_error(format!(
                "{version} is unsupported; this binary supports version {SCHEMA_VERSION}"
            )));
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
) -> Result<EffectiveConfig, Diagnostic> {
    let mut merged = ConfigDocument::parse_with_description(DEFAULT_CONFIG, "<built-in>")?
        .into_overrides("<built-in>")?;
    if let Some(user) = user {
        merge_tables(
            &mut merged,
            user.into_overrides(description)?,
            &mut Vec::new(),
        );
    }
    serde_path_to_error::deserialize(merged).map_err(Diagnostic::schema)
}

pub(crate) fn attach_source_locations(
    diagnostics: &mut [Diagnostic],
    description: &str,
    source_text: Option<&str>,
) {
    if diagnostics.is_empty() {
        return;
    }
    let source_table = source_text.and_then(|text| DeTable::parse(text).ok());
    for diagnostic in diagnostics {
        if diagnostic
            .source_name()
            .is_some_and(|source| source != description)
        {
            continue;
        }
        diagnostic.source_name = Some(description.to_owned());
        if diagnostic.source_span.is_none() {
            diagnostic.source_span = source_table
                .as_ref()
                .and_then(|table| find_span(table.get_ref(), diagnostic.field_path()));
        }
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
