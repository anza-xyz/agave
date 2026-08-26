//! Solana shred wire format: a typestate parser from raw bytes to a verified shred, and the writer
//! that produces those bytes.
//!
//! The wire format itself is documented in `README.md`. This crate is about the *order* in which a
//! shred's claims may be trusted.
//!
//! # The cascade
//!
//! A shred arrives as an opaque [`Bytes`](bytes::Bytes) buffer and advances through two states,
//! each reachable only by calling the transition that establishes it:
//!
//! ```text
//! Bytes
//!   │  parse()                  length, variant, headers        cheap: no hashing
//!   ▼  Shred<K, Parsed>
//!   │  verify(policy, leader)   policy, then Merkle root and    cheap checks first, then
//!   │                           leader signature                the expensive one
//!   ▼  Shred<K, Verified>
//!      resign(keypair)          retransmitter signature, in place
//! ```
//!
//! Because the states are uninhabited markers with no public constructors, a `Shred<_, Verified>` in
//! hand is proof that verification ran. Skipping it is a compile error rather than a bug: a shred
//! cannot be resigned on this node's authority before its leader signature was checked.
//!
//! The retransmitter signature `resign` writes is checked by
//! [`verify_retransmitter`](Shred::verify_retransmitter), which is not a transition: it asks about
//! the hop the shred took to get here, not about whether the shred is admissible, so it is available
//! in every state and no state records having asked. A repair response makes no such claim and needs
//! none.
//!
//! The policy checks and the signature check are one transition because nothing in the pipeline
//! stands between them. A sigverify worker takes one shred and runs both, cheapest first, so a state
//! in between would name a boundary no code stands on. `resign` leaves the state alone for the same
//! reason: which shreds carry a retransmitter signature is not something any consumer gates on.
//!
//! The kind ([`Data`] or [`Code`]) is likewise a type parameter, so accessors that only make sense
//! for one kind do not exist on the other. [`AnyShred`] is the kind-erased form, for the channels
//! and pipelines that carry both at once; see its documentation for where those are.
//!
//! # Provenance
//!
//! Where the bytes came from is a field, [`Provenance`], not a type parameter. It is not on the
//! wire; it is what this node knows about their origin, and it decides which door into
//! [`Verified`] was used. A shred keeps the provenance it was born with for as long as it exists.
//!
//! Four things can put a shred in [`Verified`], one per provenance:
//!
//! ```text
//! verify(policy, leader)  Received(source)   hash and ed25519, here
//! recover(data, code)     Recovered          the batch's root is checked as it is rebuilt
//! from_blockstore(bytes)  Blockstore         verified before it was ever stored
//! FecSet::build(..)       BlockProduction    signed here, over these bytes
//! ```
//!
//! So the three shortcuts that skip signature checking are not one function with a warning in its
//! doc comment; they are separate functions, and each stamps what it did. In the other direction,
//! [`resign`](Shred::resign) rejects anything but [`Provenance::Received`]: a node
//! retransmitter-signs what a peer sent it, so a shred it produced itself goes out with the
//! all-zero retransmitter signature it was built with, and one it rebuilt from an erasure batch has
//! no upstream to attest to. [`Provenance::Recovered`] being the more trusted of the two is why
//! that has to be said: trusted enough to skip verification is not the same as having something to
//! forward.
//!
//! [`Provenance::Blockstore`] is the one door that is not a single state. Every check the states
//! stand for was passed before the shred was stored and nothing there is ever unwound, so replaying
//! the cascade on the way out would pay for a signature check to learn what is already known.
//! [`from_blockstore`](Shred::from_blockstore) materializes whichever state the reading code wants,
//! with no transitions in between.
//!
//! A received shred also records which socket it arrived on, as a [`ShredSource`]. Repair and
//! Turbine differ in how the packet was solicited, not in what may be done with it, so the
//! difference gates nothing here; the stage that counts it is downstream of the socket that knows
//! it. Keeping it in the value rather than the type is what lets a batch of shreds of mixed origin
//! share a collection: blockstore insertion holds received, recovered and stored shreds together
//! and neither verifies nor resigns them, and each one can still say where it came from.
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
//! [`into_repair_response`](Shred::into_repair_response) is the other end of that nonce: the one
//! place it is written, so the two directions cannot disagree about its encoding.
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
//! use solana_shred::{AdmissionPolicy, ShredSource, fixtures, parse};
//!
//! let (parsed, repair_nonce) = parse(fixtures::DATA_SHRED, ShredSource::Turbine)?;
//! assert_eq!(repair_nonce, None);
//!
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
//! use solana_shred::Provenance;
//! assert_eq!(shred.provenance(), Provenance::Received(ShredSource::Turbine));
//!
//! // `shred.resign(..)` is reachable only from here. This fixture's variant reserves no room for a
//! // retransmitter signature, so it would hand the shred back untouched.
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Building
//!
//! The reverse direction is [`FecSet::build`], which is the write path's whole entry point. It takes
//! an erasure batch rather than a shred, because a shred's Merkle proof comes from the tree over its
//! FEC set and its signature is the leader's over that tree's root, and neither exists until all
//! 64 shreds do. What comes back is 32 data and 32 code shreds in [`Verified`], stamped
//! [`Provenance::BlockProduction`] since their signature was produced here, plus the root the next
//! batch chains to.
//!
//! Both directions are written against the same section boundaries ([`sections`]), and every shred
//! the writer produces is read back through [`ShredView`] before it is handed out, so the reader's
//! rules are the writer's test.
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

pub use crate::{
    build::{BatchPosition, FecSet, FecSetSpec},
    error::{BuildError, ParseError, RecoverError, Reject},
    headers::{AnyHeader, CodeHeader, CommonHeader, DataHeader, ShredFlags},
    kind::{Code, Data, ShredLayout},
    policy::AdmissionPolicy,
    provenance::{Provenance, ShredSource},
    recover::{Recovery, recover},
    shred::{AnyShred, CodeShred, DataShred, Shred, parse},
    shred_variant::{ShredKind, ShredVariant},
    state::{Parsed, ShredState, Verified},
    view::{AnyShredView, ShredView, ShredViewMut},
    wire_format::{Nonce, ProofEntry, Sections, sections},
};
