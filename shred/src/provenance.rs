//! Where a shred came from.
//!
//! Provenance is not on the wire and never will be: it is what *this node* knows about how the
//! bytes reached it. It is a type parameter of [`Shred`](crate::Shred) because it decides which
//! entry point into a state is legal. Reaching [`Verified`](crate::Verified) by checking a
//! signature, by having been rebuilt from a batch whose signature was already checked, by reading
//! it back from a store that only admits verified shreds, and by having produced the shred here are
//! four different claims, and only the first may be made about a shred that came off a socket. So
//! the three that skip the signature check are not one function with a warning in its doc comment;
//! they are separate constructors that a shred off a socket cannot name.
//!
//! There are exactly four origins, and a shred keeps the one it was born with for as long as it
//! exists. Which socket a received shred arrived on is deliberately not among them: repair and
//! Turbine differ in how the packet was solicited, not in what this node may do with it, and the
//! stage that cares about the difference is counting packets at the socket, where the socket is
//! known without asking the shred. Merging them is what makes a batch of received shreds one type.
//!
//! [`Recovered`] is more trusted than [`Received`], because erasure recovery runs on a batch whose
//! Merkle root was already verified, so what it returns starts out [`Verified`](crate::Verified).
//! That ordering does not extend to retransmitting: a node that rebuilt a shred it never received
//! has no upstream to attest to, so [`resign`](crate::Shred::resign) stays with [`Received`] alone.
//!
//! [`Unspecified`] is the one way out of the four, for the paths that hold shreds of mixed origin
//! and only read their fields. It is one-way, and it grants nothing.

mod sealed {
    pub trait Sealed {}
}

/// How a shred reached this node. Sealed; the provenances are exactly the five below.
pub trait Provenance: sealed::Sealed + 'static {
    /// Human-readable name, used in [`Debug`](std::fmt::Debug) output.
    const NAME: &'static str;
    /// This provenance as a runtime value, for metrics counters.
    const KIND: ProvenanceKind;
}

/// Arrived from a peer, over Turbine or as a repair response.
pub enum Received {}

/// Rebuilt by erasure recovery from the shreds of a batch that was already verified.
pub enum Recovered {}

/// Read back from this node's blockstore, which only stores verified shreds.
pub enum Stored {}

/// Built here, by this node's own block production.
pub enum SelfProduced {}

/// Provenance dropped from the type.
///
/// Reached by [`forget_provenance`](crate::Shred::forget_provenance), which is how a path that
/// holds shreds of more than one origin gets them into one collection.
pub enum Unspecified {}

/// [`Provenance`] as a value, for counters and logs.
///
/// Carried by no shred: it is read off the type parameter, so it costs nothing to have.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProvenanceKind {
    /// See [`Received`].
    Received,
    /// See [`Recovered`].
    Recovered,
    /// See [`Stored`].
    Stored,
    /// See [`SelfProduced`].
    SelfProduced,
    /// See [`Unspecified`].
    Unspecified,
}

macro_rules! impl_provenance {
    ($provenance:ident) => {
        impl sealed::Sealed for $provenance {}
        impl Provenance for $provenance {
            const NAME: &'static str = stringify!($provenance);
            const KIND: ProvenanceKind = ProvenanceKind::$provenance;
        }
    };
}

impl_provenance!(Received);
impl_provenance!(Recovered);
impl_provenance!(Stored);
impl_provenance!(SelfProduced);
impl_provenance!(Unspecified);
