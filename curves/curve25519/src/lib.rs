#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![allow(clippy::arithmetic_side_effects, clippy::op_ref)]
//! Syscall operations for curve25519

pub mod curve_syscall_traits;
pub mod edwards;
pub mod errors;
pub mod ristretto;
pub mod scalar;
