//! Where a shred came from: the markers that gate what may be done to it, and the bits that record
//! the detail.
//!
//! Provenance is not on the wire and never will be: it is what *this node* knows about how the
//! bytes reached it. It is split across two mechanisms, because the two questions it answers are
//! not the same question.
//!
//! [`Provenance`] is a type parameter of [`Shred`](crate::Shred) and decides which entry point into
//! a state is legal. Reaching [`Verified`](crate::Verified) by checking a signature, by having
//! produced the shred here, and by reading it back from a store that only admits verified shreds
//! are three different claims, and only the first may be made about a shred that came off a socket.
//! [`Received`] is the group those rules are stated over: verifying, resigning and, once it exists,
//! recovering an erasure batch all apply to the provenances that arrive from an untrusted peer.
//! Keeping the group in the type is what makes the signature-skipping constructors unnameable from
//! the receive path, rather than one function with a warning in its doc comment.
//!
//! [`ProvenanceSet`] is a field, and records the detail the rules do not depend on: which socket,
//! whether erasure recovery was involved, whether the shred round-tripped through the blockstore.
//! These are not exclusive, and the interesting cases are the combinations. A shred recovered from a
//! batch that was part Turbine and part repair carries all three bits; a shred this node built and
//! read back later carries [`ProvenanceSet::SELF_MADE`] and [`ProvenanceSet::STORED`]. No flat
//! enumeration of origins can say either, and a metrics counter wants both.
//!
//! The split is why widening is not lossy. Shreds that differ in their type parameter cannot share
//! a collection, so [`AnyReceived`] and [`Unspecified`] stand for "no longer distinguished" at the
//! type level, while the bits ride along untouched. Widening is one-way: nothing narrows back, which
//! is why it cannot be used to walk a self-produced shred into the resign path.

use std::fmt;

mod sealed {
    pub trait Sealed {}
}

/// How a shred reached this node, as far as its type is concerned. Sealed.
pub trait Provenance: sealed::Sealed + 'static {
    /// Human-readable name, used in [`Debug`] output.
    const NAME: &'static str;
}

/// A provenance that names one concrete origin, and so can seed a shred's [`ProvenanceSet`].
///
/// Only these can start a shred off: the widened markers say what a shred is no longer distinguished
/// by, which is not enough to say where it came from. That is the whole difference between building
/// a shred and retagging one.
pub trait Origin: Provenance {
    /// The bits a shred constructed at this provenance starts with.
    const BITS: ProvenanceSet;
}

/// A shred that arrived from an untrusted peer, whose signature this node has to check itself.
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

/// Arrived from a peer, over Turbine or as a repair response: which of the two the type no longer
/// says, though the shred's [`ProvenanceSet`] still does.
///
/// Reached by [`forget_source`](crate::Shred::forget_source), and what a batch assembled from both
/// sockets carries. It stays in the [`Received`] group, so the shreds keep the operations that being
/// received is what justifies: resigning, and being an input to erasure recovery.
pub enum AnyReceived {}

/// Provenance dropped from the type entirely.
///
/// Reached by [`forget_provenance`](crate::Shred::forget_provenance), for the paths that only read a
/// shred's fields. Outside [`Received`], so a shred here can be read but is no longer resignable,
/// admissible or verifiable, and it can never be widened from the provenance whose retransmitter
/// signature must stay all zeroes.
pub enum Unspecified {}

/// What is known about a shred's origin, as a set of independent facts.
///
/// A set rather than a single value because the facts compose: erasure recovery unions the sets of
/// the shreds it rebuilt from, and a shred can be both self-made and stored. Held in the shred, so
/// unlike the type parameter it survives widening.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ProvenanceSet(u8);

impl ProvenanceSet {
    /// Built here by this node's block production.
    pub const SELF_MADE: Self = Self(1 << 0);
    /// Arrived on the Turbine socket.
    pub const TURBINE: Self = Self(1 << 1);
    /// Arrived as a repair response.
    pub const REPAIR: Self = Self(1 << 2);
    /// Rebuilt by erasure recovery, rather than received as itself.
    pub const RECOVERED: Self = Self(1 << 3);
    /// Round-tripped through the blockstore.
    pub const STORED: Self = Self(1 << 4);

    /// Bits set by arriving from somewhere other than this node.
    const EXTERNAL: Self = Self(Self::TURBINE.0 | Self::REPAIR.0 | Self::RECOVERED.0);

    const NAMED: [(Self, &'static str); 5] = [
        (Self::SELF_MADE, "SELF_MADE"),
        (Self::TURBINE, "TURBINE"),
        (Self::REPAIR, "REPAIR"),
        (Self::RECOVERED, "RECOVERED"),
        (Self::STORED, "STORED"),
    ];

    /// Nothing known.
    #[inline]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Everything either set knows.
    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every bit of `other` is set here.
    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any bit of `other` is set here.
    #[inline]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether the shred is known to have come from another node.
    ///
    /// Derived rather than stored, so it cannot disagree with the bits it is derived from. Positive
    /// evidence only: a set carrying just [`Self::STORED`] answers `false` because it does not say
    /// where the bytes were before they were stored, not because they were made here.
    #[inline]
    pub const fn is_external(self) -> bool {
        self.intersects(Self::EXTERNAL)
    }

    /// The raw bits, for a metrics tag.
    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl fmt::Debug for ProvenanceSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("empty");
        }
        let mut first = true;
        for (bit, name) in Self::NAMED {
            if self.contains(bit) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

macro_rules! impl_provenance {
    ($provenance:ident) => {
        impl sealed::Sealed for $provenance {}
        impl Provenance for $provenance {
            const NAME: &'static str = stringify!($provenance);
        }
    };
    ($provenance:ident, $bits:expr) => {
        impl_provenance!($provenance);
        impl Origin for $provenance {
            const BITS: ProvenanceSet = $bits;
        }
    };
}

impl_provenance!(SelfProduced, ProvenanceSet::SELF_MADE);
impl_provenance!(Blockstore, ProvenanceSet::STORED);
impl_provenance!(Recovered, ProvenanceSet::RECOVERED);
impl_provenance!(TurbineRx, ProvenanceSet::TURBINE);
impl_provenance!(RepairRx, ProvenanceSet::REPAIR);
impl_provenance!(AnyReceived);
impl_provenance!(Unspecified);

impl Received for TurbineRx {}
impl Received for RepairRx {}
impl Received for AnyReceived {}
