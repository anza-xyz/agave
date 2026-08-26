//! The validation states a shred passes through.
//!
//! A shred arrives as an opaque [`Bytes`](bytes::Bytes) buffer and advances through two states,
//! each reachable only by calling the transition that establishes it:
//!
//! ```text
//! Bytes
//!   │  parse_turbine()          length, variant, headers        cheap: no hashing
//!   │  parse_repair()           the same, plus the nonce
//!   ▼  Shred<K, Parsed>
//!   │  verify(policy, leader)   policy, then Merkle root and    cheap checks first, then
//!   │                           leader signature                the expensive one
//!   ▼  Shred<K, Verified>
//!      resign(keypair)          retransmitter signature, state unchanged
//! ```
//!
//!
//! The Kind ([`Data`](crate::kind::Data) or [`Code`](crate::kind::Code)) is likewise a type
//! parameter, so accessors that only make sense for one kind do not exist on the other, and
//! [`AnyShred`](crate::shred::AnyShred) is the kind-erased form for the channels that carry both.
//! See [`kind`] for why, and `AnyShred` itself for where those channels are.
//!
mod sealed {
    pub trait Sealed {}
}

/// A stage of the validation cascade.
pub trait ShredState: sealed::Sealed {
    /// Human-readable name of the state, used in `Debug` output.
    const NAME: &'static str;
}

/// The shred's length and headers are well-formed. Nothing about its content is known.
pub enum Parsed {}

/// The leader's signature over the shred's Merkle root verifies, and the headers agreed with the
/// caller's admission policy on the way.
pub enum Verified {}

macro_rules! impl_state {
    ($state:ident) => {
        impl sealed::Sealed for $state {}
        impl ShredState for $state {
            const NAME: &'static str = stringify!($state);
        }
    };
}

impl_state!(Parsed);
impl_state!(Verified);
