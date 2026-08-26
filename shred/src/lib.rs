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
//! let shred = shred.verify(&policy, &fixtures::leader())?;
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
//! # Building
//!
//! The reverse direction is [`FecSet::build`](crate::build::FecSet::build), which is the write
//! path's whole entry point. It takes an erasure batch rather than a shred, for the reason
//! [`build`] gives. What comes back is 32 data and 32 code shreds in
//! [`Verified`](crate::state::Verified), stamped
//! [`Provenance::BlockProduction`](crate::provenance::Provenance::BlockProduction) since their
//! signature was produced here, plus the root the next batch chains to.
//!
//! Both directions are written against the same section boundaries
//! ([`sections`](crate::wire_format::sections)), and every shred the writer produces is read back
//! through [`ShredView`](crate::view::ShredView) before it is handed out, so the reader's rules are
//! the writer's test.
//!
//! # Status
//!
//! A draft: nothing else in the tree depends on it yet. Its output is byte-identical to
//! `solana-ledger`'s shredder for the batches it can build, which its tests assert directly.
//! Splitting a slot's data across batches is not here; that is block production's business.
//! Deshredding a batch back into ledger entries is not here either, nor is any identifier for a
//! shred or an erasure set.

pub mod build;
pub mod error;
#[cfg(feature = "dev-context-only-utils")]
pub mod fixtures;
pub mod headers;
pub mod kind;
pub mod merkle;
pub mod policy;
pub mod provenance;
pub mod recover;
pub mod shred;
pub mod shred_variant;
pub mod state;
pub mod view;
pub mod wire_format;
