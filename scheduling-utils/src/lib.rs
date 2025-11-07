#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
pub mod error;
pub mod thread_aware_account_locks;

#[cfg(unix)]
pub mod handshake;
#[cfg(unix)]
pub mod pubkeys_ptr;
#[cfg(unix)]
pub mod responses_region;
#[cfg(unix)]
pub mod transaction_ptr;
