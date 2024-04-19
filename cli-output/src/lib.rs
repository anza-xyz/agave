#![allow(clippy::arithmetic_side_effects)]
mod cli_output;
pub mod cli_version;
pub mod display;
pub use cli_output::*;

#[doc(hidden)]
// Crates used by macros.  Reexporting them here allows macros to be invoked in any context, without
// requiring the users to import specific crates and/or modules.
pub mod reexport {
    pub use serde_json;
}
