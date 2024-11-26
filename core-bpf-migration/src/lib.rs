//! Solana migration of builtins to Core BPF.
//!
//! Warning: This crate is not for public consumption. It will change, and
//! could possibly be removed altogether in the future. For now, it is purely
//! for the purpose of managing the migration of builtins to Core BPF.
//!
//! It serves as a source of truth for:
//! * The list of builtins that a Bank should add.
//! * Which of those builtins have been assigned a feature gate to migrate to
//!   Core BPF, as well as whether or not that feature gate has been activated.
//! * Which builtins should still reserve `DEFAULT_COMPUTE_UNITS` in the cost
//!   model.

pub mod callback;
