use ahash::HashSet;
use solana_sdk::hash::Hash;
use solana_sdk::message::AddressLoader;
use solana_sdk::packet::Packet;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::{SanitizedTransaction, SanitizedVersionedTransaction};
use std::error::Error;

/// DeserializablePacket can be deserialized from a Packet.
///
/// DeserializablePacket will be deserialized as a SanitizedTransaction
/// to be scheduled in transaction stream and scheduler.
pub trait DeserializableTxPacket: PartialEq + PartialOrd + Eq + Sized {
    type DeserializeError: Error;

    fn from_packet(packet: Packet) -> Result<Self, Self::DeserializeError>;

    /// This function deserializes packets into transactions,
    /// computes the blake3 hash of transaction messages.
    fn build_sanitized_transaction(
        &self,
        votes_only: bool,
        address_loader: impl AddressLoader,
        reserved_account_keys: &HashSet<Pubkey>,
    ) -> Option<SanitizedTransaction>;

    fn original_packet(&self) -> &Packet;

    /// deserialized into versionedTx, and then to SanitizedTransaction.
    fn transaction(&self) -> &SanitizedVersionedTransaction;

    fn message_hash(&self) -> &Hash;

    fn is_simple_vote(&self) -> bool;

    fn compute_unit_price(&self) -> u64;

    fn compute_unit_limit(&self) -> u64;
}
