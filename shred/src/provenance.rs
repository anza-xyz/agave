//! Where a shred came from.
//!
//! Provenance is not on the wire and never will be: it is what *this node* knows about how the
//! bytes reached it. It is a type parameter of [`Shred`](crate::Shred) because it decides which
//! entry point into a state is legal. Reaching [`Verified`](crate::Verified) by checking a
//! signature, by having produced the shred here, and by reading it back from a store that only
//! admits verified shreds are three different claims, and only the first one may be made about a
//! shred that came off a socket.
//!
//! [`Received`] is the group the crate cares about. Verifying, resigning and, once it exists,
//! recovering an erasure batch all apply to exactly the two provenances that arrive from an
//! untrusted peer.
//!
//! Shreds of different provenance cannot share a `Vec`, so there are two markers that stand for
//! "no longer distinguished": [`AnyReceived`] within the [`Received`] group, and [`Unspecified`]
//! outside it. Widening to them is one-way. Nothing narrows back, which is why widening cannot be
//! used to smuggle a self-produced shred into the resign path.

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

/// A shred that arrived over the network, whose signature this node has to check itself.
pub trait Received: Provenance {}

/// Built here, by this node's own block production.
pub enum SelfProduced {}

/// Read back from this node's blockstore, which only stores verified shreds.
pub enum Blockstore {}

/// Reconstructed by erasure recovery from the received shreds of its batch.
///
/// Nothing produces this yet: recovery is not part of the crate. The marker exists so that when it
/// arrives, the shreds it returns are distinguishable from the ones it was given.
pub enum Recovered {}

/// Arrived on the Turbine socket.
pub enum TurbineRx {}

/// Arrived as a repair response, answering a request this node sent.
pub enum RepairRx {}

/// Arrived from a peer, over Turbine or as a repair response: which of the two is no longer
/// tracked.
///
/// Reached by [`forget_source`](crate::Shred::forget_source), and the provenance a batch assembled
/// from both sockets carries. It stays in the [`Received`] group, so the shreds keep the operations
/// that being received is what justifies: resigning, and being an input to erasure recovery.
pub enum AnyReceived {}

/// Provenance dropped entirely.
///
/// Reached by [`forget_provenance`](crate::Shred::forget_provenance), for the paths that only read
/// a shred's fields and have no reason to care where it came from. Outside [`Received`], so a shred
/// here can be read but is no longer resignable, and it can never be widened from the provenance
/// whose retransmitter signature must stay all zeroes.
pub enum Unspecified {}

/// [`Provenance`] as a value, for counters and logs.
///
/// Carried by no shred: it is read off the type parameter, so it costs nothing to have.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProvenanceKind {
    /// See [`SelfProduced`].
    SelfProduced,
    /// See [`Blockstore`].
    Blockstore,
    /// See [`Recovered`].
    Recovered,
    /// See [`TurbineRx`].
    TurbineRx,
    /// See [`RepairRx`].
    RepairRx,
    /// See [`AnyReceived`].
    AnyReceived,
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

impl_provenance!(SelfProduced);
impl_provenance!(Blockstore);
impl_provenance!(Recovered);
impl_provenance!(TurbineRx);
impl_provenance!(RepairRx);
impl_provenance!(AnyReceived);
impl_provenance!(Unspecified);

impl Received for TurbineRx {}
impl Received for RepairRx {}
impl Received for AnyReceived {}
