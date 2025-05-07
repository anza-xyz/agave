/// A trait to see if an account's balance is zero lamports or not.
pub trait IsZeroLamport {
    /// Is this a zero-lamport account?
    fn is_zero_lamport(&self) -> bool;
}
