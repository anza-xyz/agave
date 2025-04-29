#![cfg(feature = "agave-unstable-api")]
//! Solana shred wire format: a typestate parser from raw bytes to a verified shred, and the writer
//! that produces those bytes. The wire format itself is documented in `README.md`.
//!
//! # Example
//!
//! ```
//! use solana_shred::{
//!     fixtures, policy::AdmissionPolicy, provenance::ShredSource, shred::parse_turbine,
//! };
//!
//! let parsed = parse_turbine(fixtures::DATA_SHRED)?;
//! let shred = parsed.into_data().expect("the fixture is a data shred");
//! let policy = AdmissionPolicy {
//!     shred_version: shred.version(),
//!     root: shred.slot() - 1,
//!     max_slot: shred.slot() + 1_000,
//!     max_data_shreds_per_slot: 32_768,
//!     max_code_shreds_per_slot: 32_768,
//! };
//!
//! let shred = shred.check_policy(&policy)?.verify(&fixtures::leader())?;
//! assert_eq!(shred.data().len(), 963);
//!
//! use solana_shred::provenance::Provenance;
//! assert_eq!(shred.provenance(), Provenance::Received(ShredSource::Turbine));
//!
//! // `shred.resign(..)` is reachable only from here. This fixture's variant reserves no room for a
//! // retransmitter signature, so it would hand the shred back untouched.
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Parsing
//! Parsing materializes only the header scalars: 19 bytes of common header plus 5 or 6 for the
//! kind's own. Everything else stays in the buffer and is handed out by
//! [`ShredView`](crate::view::ShredView) as a reference into it: the signature, the body, the
//! chained Merkle root, the proof entries, the retransmitter signature. Deriving a view hashes
//! nothing and copies nothing, so an accessor costs a fraction of the parse that produced the
//! shred; [`view`] has the argument for lending the sections out rather than splitting them off
//! once.
//!
//! Every rule about the bytes (the payload length, the kind the variant byte selects, whether a
//! repair nonce follows) is applied there, so no check is made twice and no caller can forget one.
//!
//! # Building
//!
//! The reverse direction is [`FecSet::build`](crate::shredder::FecSet::build), which is the write
//! path's whole entry point. It takes an erasure batch rather than a shred, for the reason
//! [`shredder`] gives. What comes back is 32 data and 32 code shreds in
//! [`Verified`](crate::state::Verified), stamped
//! [`Provenance::BlockProduction`](crate::provenance::Provenance::BlockProduction) since their
//! signature was produced here, plus the root the next batch chains to.
//!
//! Both directions are written against the same section boundaries
//! ([`sections`](crate::constants::sections)), and every shred the writer produces is read back
//! through [`ShredView`](crate::view::ShredView) before it is handed out, so the reader's rules are
//! the writer's test.

pub mod error;
#[cfg(feature = "dev-context-only-utils")]
pub mod fixtures;
pub mod policy;
pub mod provenance;
pub mod recover;
pub mod shred;
pub mod shredder;
pub mod state;

pub use {
    solana_shred_verify::merkle,
    solana_shred_wire_format::{constants, headers, kind, shred_variant, view},
};
