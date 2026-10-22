use crate::is_zero_lamport::IsZeroLamport;

/// A trait to see if an account is loadable or not.
pub trait IsLoadable {
    /// Is this account loadable?
    fn is_loadable(&self) -> bool;
}

impl<T: IsZeroLamport> IsLoadable for T {
    fn is_loadable(&self) -> bool {
        // Don't ever load zero lamport accounts into runtime because
        // the existence of zero-lamport accounts are never deterministic!!
        !self.is_zero_lamport()
    }
}
