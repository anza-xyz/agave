#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
pub(crate) mod solana {
    pub(crate) mod wen_restart_proto {
        include!(concat!(env!("OUT_DIR"), "/solana.wen_restart_proto.rs"));
    }
}

pub(crate) mod heaviest_fork_aggregate;
pub(crate) mod last_voted_fork_slots_aggregate;
pub mod wen_restart;
