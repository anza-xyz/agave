//! The validation states a shred passes through.
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

/// The headers agree with the caller's admission policy: right cluster, plausible slot, index and
/// FEC set, self-consistent flags. No cryptography has been checked.
pub enum Admissible {}

/// The leader's signature over the shred's Merkle root verifies.
pub enum Verified {}

/// A retransmitter signature has been written over the verified Merkle root.
pub enum Resigned {}

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
impl_state!(Resigned);
