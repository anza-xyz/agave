//! Solana shred wire format: a typestate parser from raw bytes to a verified shred, and the writer
//! that produces those bytes.
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
//! The kind ([`Data`] or [`Code`]) is likewise a type parameter, so accessors that only make
//! sense for one kind do not exist on the other. [`ShredParsed`] is the single point where the kind
//! is still a runtime tag, because [`parse`] cannot know it before reading the variant byte.
//!
//! # Cost
//!
//! Parsing materializes only the header scalars: 19 bytes of common header plus 5 or 6 for the
//! kind's own. Everything else stays in the buffer and is handed out by [`ShredView`] as a
//! reference into it: the signature, the body, the chained Merkle root, the proof entries, the
//! retransmitter signature.
//!
//! Every rule about the bytes (the payload length, the kind the variant byte selects, the optional
//! trailing repair nonce) is applied by [`ShredView::read_packet`], so no check is made twice.
//!
//! No byte offset is stored, and only one ([`OFFSET_OF_VARIANT`](wire_format::OFFSET_OF_VARIANT), needed
//! to pick a kind before there is anything to walk) is written down. [`ShredView::read`] walks the
//! sections in wire order with wincode, taking each from the reader as it comes, so the reader's
//! cursor is the offset. Section *sizes* likewise come from the wincode schemas of the types that
//! occupy them, not from literals.
//!
//! # Example
//!
//! ```
//! use solana_shred::{AdmissionPolicy, ShredParsed, fixtures, parse};
//!
//! let (parsed, repair_nonce) = parse(fixtures::DATA_SHRED)?;
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
//! let shred = shred.verify(&fixtures::leader())?;
//! assert_eq!(shred.data()?.len(), 963);
//!
//! // `shred.resign(..)` is reachable only from here, and only for resigned variants.
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Building
//!
//! The reverse direction is [`FecSet::build`], which is the write path's whole entry point. It takes
//! an erasure batch rather than a shred, because a shred's Merkle proof comes from the tree over its
//! FEC set and its signature is the leader's over that tree's root, and neither exists until all
//! 64 shreds do. What comes back is 32 data and 32 code shreds in [`Verified`], since their signature
//! was produced here, plus the root the next batch chains to.
//!
//! Both directions are written against the same section boundaries ([`sections`]), and every shred
//! the writer produces is read back through [`ShredView`] before it is handed out, so the reader's
//! rules are the writer's test.
//!
//! # Status
//!
//! A draft: nothing else in the tree depends on it yet. Its output is byte-identical to
//! `solana-ledger`'s shredder for the batches it can build, which its tests assert directly.
//! Splitting a slot's data across batches is not here (that is block production's business), and
//! neither is erasure recovery of a batch that arrived incomplete.

pub mod build;
pub mod error;
#[cfg(feature = "dev-context-only-utils")]
pub mod fixtures;
pub mod headers;
pub mod kind;
pub mod merkle;
pub mod policy;
pub mod shred;
pub mod shred_variant;
pub mod state;
pub mod view;
pub mod wire_format;

pub use crate::{
    build::{FecSet, FecSetSpec},
    error::{BuildError, InvalidDataSize, ParseError, Reject},
    headers::{CodeHeader, CommonHeader, DataHeader, ShredFlags},
    kind::{Code, Data, ShredKind},
    policy::AdmissionPolicy,
    shred::{CodeShred, DataShred, Shred, ShredParsed, parse},
    shred_variant::{ShredType, ShredVariant},
    state::{Admissible, Parsed, Resigned, ShredState, Verified},
    view::{ShredView, ShredViewMut},
    wire_format::{Nonce, ProofEntry, Sections, sections},
};
