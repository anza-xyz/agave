//! Defines different types of Pubkeys.
use {solana_pubkey::Pubkey, std::fmt::Display};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// A wrapper to help distinguide node identity pubkeys from other types of pubkeys.
pub struct NodePubkey(pub Pubkey);

impl Display for NodePubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
