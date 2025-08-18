use {
    clap::{Arg, ArgMatches},
    thiserror::Error,
};

/// Convenience trait for small types that can be converted to a str (e.g., bool).
///
/// Particularly useful when using [`ArgExt::default_value_if_is_some`] with with some type
/// that can be trivially represented as a str, and may do so often.
pub trait AsStrExt<'a> {
    fn as_str(&self) -> &'a str;
}

impl AsStrExt<'static> for bool {
    fn as_str(&self) -> &'static str {
        match self {
            true => "true",
            false => "false",
        }
    }
}

/// Extension trait for `Arg` to add additional functionality.
pub trait ArgExt<'a> {
    /// Set the default value if the given `default` argument is `Some`.
    fn default_value_if_is_some(self, default: Option<&'a str>) -> Self;
}

impl<'a> ArgExt<'a> for Arg<'a, '_> {
    fn default_value_if_is_some(self, default: Option<&'a str>) -> Self {
        match default {
            Some(default) => self.default_value(default),
            None => self,
        }
    }
}

#[derive(Error, Debug)]
pub enum ArgMatchesExtError<'a> {
    #[error("Invalid boolean value for {name}: {value}")]
    InvalidBool { name: &'a str, value: &'a str },
}

/// Extension trait for `ArgMatches` to add additional functionality.
pub trait ArgMatchesExt<'a> {
    /// Check if the given flag is set.
    fn check_flag(&'a self, name: &'a str) -> Result<bool, ArgMatchesExtError<'a>>;
}

impl<'a> ArgMatchesExt<'a> for ArgMatches<'a> {
    /// Check if the given flag is set.
    ///
    /// Useful when providing default values for arguments configured
    /// as flags, i.e., via `Arg::takes_value(false)`. This will be a common
    /// occurrence when populating default CLI arguments from a config file.
    ///
    /// Clap version 2.x doesn't trivially support default flag values.
    /// In particular, calling [`Arg::default_value`] implicitly sets `Arg::takes_value(true)`.
    /// This is problematic for the typical idiom for checking if a flag is present,
    /// [`ArgMatches::is_present`], as it will always return true if a default is set.
    ///
    /// To work around this, we use [`ArgMatches::occurrences_of`] to check if the flag
    /// was explicitly set on the command line, as this will not be affected by a default value.
    /// Then, if no occurrences are found, we check the argument value for `"true"` or `"false"`,
    /// as this _will_ be set by the default value.
    fn check_flag(&'a self, name: &'a str) -> Result<bool, ArgMatchesExtError<'a>> {
        // Check if the flag was explicitly set on the command line.
        if self.occurrences_of(name) > 0 {
            return Ok(true);
        }

        // If the flag was not explicitly set, check the argument value, which may have been
        // provided as a default.
        match self.value_of(name).map(|s| s.trim()) {
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            Some(value) => Err(ArgMatchesExtError::InvalidBool { name, value }),
            // Command line flag was not set, and no default was provided.
            None => Ok(false),
        }
    }
}
