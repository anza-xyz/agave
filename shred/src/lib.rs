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
//! # Provenance
//!
//! The third parameter is where the bytes came from: [`Received`], [`Recovered`], [`Stored`] or
//! [`SelfProduced`]. It is not on the wire; it is what this node knows about their origin, and it
//! decides which door into a state is open. A shred keeps the provenance it was born with for as
//! long as it exists.
//!
//! Four things can put a shred in [`Verified`], one per provenance:
//!
//! ```text
//! Shred<K, Admissible, Received>::verify(leader)   hash and ed25519, here
//! (erasure recovery)                 -> Recovered      the batch's root was verified already
//! Shred<K, S, Stored>::from_blockstore(..)          verified before it was ever stored
//! FecSet::build(..)                  -> SelfProduced   signed here, over these bytes
//! ```
//!
//! So the three shortcuts that skip signature checking are not one function with a warning in its
//! doc comment; they are separate functions that a shred off a socket cannot name. In the other
//! direction, [`resign`](Shred::resign) belongs to [`Received`] and nothing else: a node
//! retransmitter-signs what a peer sent it, so a shred it produced itself goes out with the
//! all-zero retransmitter signature it was built with, and one it rebuilt from an erasure batch has
//! no upstream to attest to. [`Recovered`] being the more trusted of the two is why that has to be
//! said: trusted enough to skip verification is not the same as having something to forward.
//!
//! [`Stored`] is the one door that is not a single state. Every check the states stand for was
//! passed before the shred was stored and nothing there is ever unwound, so replaying the cascade
//! on the way out would pay for a signature check to learn what is already known.
//! `from_blockstore` materializes whichever state the reading code wants, with no transitions in
//! between.
//!
//! Which socket a received shred arrived on is deliberately not part of this. Repair and Turbine
//! differ in how the packet was solicited, not in what may be done with it, and the stage that
//! counts the difference is at the socket, where the socket is known without asking the shred.
//! Merging them is what makes a batch of received shreds a single type.
//!
//! Provenance is a type parameter, so shreds that differ in it are different types and cannot share
//! a collection. [`forget_provenance`](Shred::forget_provenance) widens any shred to
//! [`Unspecified`], which is what a path holding shreds of mixed origin needs: blockstore insertion
//! takes received and recovered shreds together and neither verifies, admits nor resigns them. The
//! widening is one-way and grants nothing, so anything that reports the origin reads
//! [`provenance`](Shred::provenance) before widening.
//!
//! [`provenance`](crate::provenance) is where the markers and the [`Received`] group live.
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
//! assert_eq!(shred.provenance(), solana_shred::ProvenanceKind::Received);
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
//! 64 shreds do. What comes back is 32 data and 32 code shreds in [`Verified`] and [`SelfProduced`],
//! since their signature was produced here, plus the root the next batch chains to.
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
pub mod provenance;
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
    provenance::{
        Provenance, ProvenanceKind, Received, Recovered, SelfProduced, Stored, Unspecified,
    },
    shred::{CodeShred, DataShred, Shred, ShredParsed, parse},
    shred_variant::{ShredType, ShredVariant},
    state::{Admissible, Parsed, Resigned, ShredState, Verified},
    view::{ShredView, ShredViewMut},
    wire_format::{Nonce, ProofEntry, Sections, sections},
};
