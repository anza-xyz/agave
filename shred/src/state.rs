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
