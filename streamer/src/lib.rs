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
pub mod evicting_sender;
pub mod msghdr;
pub mod nonblocking;
pub mod packet;
pub mod quic;
pub mod recvmmsg;
pub mod sendmmsg;
pub mod streamer;

#[macro_use]
extern crate log;

#[macro_use]
extern crate solana_metrics;
