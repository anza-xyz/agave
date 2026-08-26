//! The validation states a shred passes through.
//!
//! A shred arrives as an opaque [`Bytes`](bytes::Bytes) buffer and advances through three states,
//! each reachable only by calling the transition that establishes it:
//!
//! ```text
//! Bytes
//!   │  parse_turbine()        length, variant, headers          cheap
//!   │  parse_repair()         the same, plus the nonce
//!   ▼  Shred<K, Parsed>
//!   │  check_policy(policy)   the headers against this node's   cheap
//!   │                         current view of the cluster
//!   ▼  Shred<K, Admissible>
//!   │  verify(leader)         the Merkle root the proof         the expensive stage
//!   │                         reconstructs, then the signature
//!   ▼  Shred<K, Verified>
//!      resign(keypair)        retransmitter signature, state unchanged
//! ```

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

/// The headers agree with the admission policy this node held, so the shred is worth verifying.
/// Nothing about the shred's authenticity is known.
pub enum Admissible {}

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
impl_state!(Admissible);
impl_state!(Verified);
