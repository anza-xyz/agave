//! Program state
use solana_bls_signatures::Pubkey as BlsPubkey;

/// The accounting and vote information associated with
/// this vote account
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VoteState {
    /// Associated BLS public key
    pub(crate) bls_pubkey: BlsPubkey,
}
