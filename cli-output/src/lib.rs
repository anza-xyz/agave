#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![allow(clippy::arithmetic_side_effects)]
mod cli_output;
pub mod cli_version;
pub mod display;
pub use cli_output::*;

pub trait QuietDisplay: std::fmt::Display {
    fn write_str(&self, w: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(w, "{self}")
    }
}

pub trait VerboseDisplay: std::fmt::Display {
    fn write_str(&self, w: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(w, "{self}")
    }
}
