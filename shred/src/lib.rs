//! Solana shred wire format: a typestate parser from raw bytes to a verified shred.
//!
//! The wire format itself is documented in `README.md`. This crate is about the *order* in which a
//! shred's claims may be trusted.
//!
//! # The cascade
//!
//! A shred arrives as an opaque [`Bytes`](bytes::Bytes) buffer and advances through four states,
//! each reachable only by calling the transition that establishes it:
//!
//! ```text
//! Bytes
//!   │  parse()          length, variant, headers          cheap: no hashing
//!   ▼  Shred<K, Parsed>
//!   │  admit(policy)    slot, index, FEC set, flags       cheap: no cryptography
//!   ▼  Shred<K, Admissible>
//!   │  verify(leader)   Merkle root, leader signature     expensive
//!   ▼  Shred<K, Verified>
//!   │  resign(keypair)  retransmitter signature
//!   ▼  Shred<K, Resigned>
//! ```
//!
//! Because the states are uninhabited markers with no public constructors, a `Shred<_, Verified>` in
//! hand is proof that verification ran. Skipping a stage is a compile error rather than a bug: a
//! shred cannot be resigned on this node's authority before its leader signature was checked.
//!
//! The kind — [`Data`] or [`Code`] — is likewise a type parameter, so accessors that only make
//! sense for one kind do not exist on the other. [`ShredParsed`] is the single point where the kind
//! is still a runtime tag, because [`parse`] cannot know it before reading the variant byte.
//!
//! # Cost
//!
//! Parsing materializes only the header scalars: 19 bytes of common header plus 5 or 6 for the
//! kind's own. Everything else stays in the buffer and is handed out by [`ShredView`] as a
//! reference into it — the signature, the body, the chained Merkle root, the proof entries, the
//! retransmitter signature.
//!
//! No byte offset is stored, and only one ([`OFFSET_OF_VARIANT`](layout::OFFSET_OF_VARIANT), needed
//! to pick a kind before there is anything to walk) is written down. [`ShredView::read`] walks the
//! sections in wire order with wincode, taking each from the reader as it comes, so the reader's
//! cursor is the offset. Section *sizes* likewise come from the wincode schemas of the types that
//! occupy them, not from literals.
//!
//! # Example
//!
//! ```
//! use solana_shred::{AdmissionPolicy, ShredParsed, fixture, parse};
//!
//! let (parsed, repair_nonce) = parse(fixture::data_shred())?;
//! assert_eq!(repair_nonce, None);
//!
//! let ShredParsed::Data(shred) = parsed else {
//!     panic!("the fixture is a data shred");
//! };
//! let policy = AdmissionPolicy {
//!     shred_version: shred.version(),
//!     root: shred.slot() - 1,
//!     max_slot: shred.slot() + 1_000,
//!     max_data_shreds_per_slot: 32_768,
//!     max_code_shreds_per_slot: 32_768,
//! };
//!
//! let shred = shred.admit(&policy)?;
//! let shred = shred.verify(&fixture::leader())?;
//! assert_eq!(shred.data()?.len(), 963);
//!
//! // `shred.resign(..)` is reachable only from here, and only for resigned variants.
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Status
//!
//! A draft. It parses and validates headers, but does not construct shreds, and nothing else in the
//! tree depends on it yet. Merkle handling is unfinished: [`merkle`] checks the proof region's
//! *shape* and hashes nothing, so [`verify`](Shred::verify) authenticates nothing and
//! [`resign`](Shred::resign) signs the leaf region rather than the root.

pub mod error;
#[cfg(feature = "dev-context-only-utils")]
pub mod fixture;
pub mod header;
pub mod kind;
pub mod layout;
pub mod merkle;
pub mod policy;
pub mod shred;
pub mod shred_variant;
pub mod state;
pub mod view;

pub use crate::{
    error::{InvalidDataSize, ParseError, Reject},
    header::{CodeHeader, CommonHeader, DataHeader, ShredFlags},
    kind::{Code, Data, ShredKind},
    layout::ProofEntry,
    policy::AdmissionPolicy,
    shred::{CodeShred, DataShred, Nonce, Shred, ShredParsed, parse},
    shred_variant::{ShredType, ShredVariant},
    state::{Admissible, Parsed, Resigned, ShredState, Verified},
    view::ShredView,
};
