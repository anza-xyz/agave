//! Structured diagnostics for validator configuration and policy resolution.

use {
    crate::interface::format_field_key,
    std::{fmt, io, ops::Range},
};

/// The operation or rule that produced a diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// The configuration file could not be read.
    Io,
    /// The input is invalid TOML.
    TomlSyntax,
    /// The schema version is missing, malformed, or unsupported.
    SchemaVersion,
    /// The effective TOML does not match the configuration schema.
    Schema,
    /// A field violates a validation constraint.
    InvalidValue,
    /// A field refers to an undeclared interface or queue.
    InvalidReference,
    /// A host CPU constraint prevented XDP policy resolution.
    CpuResolution,
    /// A declared worker queue is unused.
    InactiveXdpQueue,
    /// A document could not be serialized to TOML.
    Serialization,
}

/// One step in a path through a TOML document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldPathSegment {
    /// A table key, including its full contents if the key was quoted.
    Key(String),
    /// A zero-based array index.
    Index(usize),
}

/// An error or warning with optional source information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    kind: DiagnosticKind,
    field_path: Vec<FieldPathSegment>,
    message: DiagnosticMessage,
    pub(crate) source_name: Option<String>,
    pub(crate) source_span: Option<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DiagnosticMessage {
    Text(String),
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    TomlSyntax(Box<toml::de::Error>),
}

impl Diagnostic {
    /// What produced this diagnostic.
    pub fn kind(&self) -> DiagnosticKind {
        self.kind
    }

    /// The underlying file read error kind, when available.
    pub fn io_error_kind(&self) -> Option<io::ErrorKind> {
        match &self.message {
            DiagnosticMessage::Io { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// The affected field, from the document root. An empty slice denotes
    /// a diagnostic without a field path.
    pub fn field_path(&self) -> &[FieldPathSegment] {
        &self.field_path
    }

    /// The input name, such as the loaded file's path.
    pub fn source_name(&self) -> Option<&str> {
        self.source_name.as_deref()
    }

    /// The half-open UTF-8 byte range in the original TOML, when available.
    pub fn source_span(&self) -> Option<Range<usize>> {
        self.source_span.clone()
    }

    /// The human-readable diagnostic text, without the file or field path prefix.
    pub fn message(&self) -> &str {
        match &self.message {
            DiagnosticMessage::Text(message) | DiagnosticMessage::Io { message, .. } => message,
            DiagnosticMessage::TomlSyntax(error) => error.message(),
        }
    }

    pub(crate) fn new(kind: DiagnosticKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            field_path: Vec::new(),
            message: DiagnosticMessage::Text(message.into()),
            source_name: None,
            source_span: None,
        }
    }

    /// Attach a field path or prefix an existing relative path.
    pub(crate) fn at_field<'a>(mut self, path: impl IntoIterator<Item = &'a str>) -> Self {
        self.field_path.splice(
            ..0,
            path.into_iter()
                .map(|key| FieldPathSegment::Key(key.into())),
        );
        self
    }

    pub(crate) fn io(error: &io::Error, source: &str) -> Self {
        Self {
            kind: DiagnosticKind::Io,
            field_path: Vec::new(),
            message: DiagnosticMessage::Io {
                kind: error.kind(),
                message: error.to_string(),
            },
            source_name: Some(source.to_owned()),
            source_span: None,
        }
    }

    pub(crate) fn toml_syntax(error: toml::de::Error, source: &str) -> Self {
        Self {
            kind: DiagnosticKind::TomlSyntax,
            field_path: Vec::new(),
            source_name: Some(source.to_owned()),
            source_span: error.span(),
            message: DiagnosticMessage::TomlSyntax(Box::new(error)),
        }
    }

    pub(crate) fn schema(error: serde_path_to_error::Error<toml::de::Error>) -> Self {
        use serde_path_to_error::Segment;

        let mut diagnostic = Self::new(DiagnosticKind::Schema, error.inner().message());
        diagnostic.field_path = error
            .path()
            .iter()
            .map_while(|segment| match segment {
                Segment::Map { key } | Segment::Enum { variant: key } => {
                    Some(FieldPathSegment::Key(key.clone()))
                }
                Segment::Seq { index } => Some(FieldPathSegment::Index(*index)),
                Segment::Unknown => None,
            })
            .collect();
        diagnostic
    }

    fn write_field_path(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (position, segment) in self.field_path.iter().enumerate() {
            match segment {
                FieldPathSegment::Key(key) => {
                    if position > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(&format_field_key(key))?;
                }
                FieldPathSegment::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(source) = self.source_name() {
            match self.kind {
                DiagnosticKind::Io => write!(f, "failed to read config file `{source}`: ")?,
                DiagnosticKind::InactiveXdpQueue => (),
                _ => write!(f, "invalid config file `{source}`: ")?,
            }
        }
        if !self.field_path.is_empty()
            && matches!(
                self.kind,
                DiagnosticKind::SchemaVersion
                    | DiagnosticKind::InvalidValue
                    | DiagnosticKind::InvalidReference
            )
        {
            self.write_field_path(f)?;
            f.write_str(" ")?;
        }
        match &self.message {
            DiagnosticMessage::TomlSyntax(error) => write!(f, "{error}")?,
            _ => f.write_str(self.message())?,
        }
        if self.kind == DiagnosticKind::Schema && !self.field_path.is_empty() {
            f.write_str("\nin `")?;
            self.write_field_path(f)?;
            f.write_str("`")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.message {
            DiagnosticMessage::TomlSyntax(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}
