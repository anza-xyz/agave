//! Solana Priority Graph Scheduler.
pub mod id_generator;
pub mod in_flight_tracker;
pub mod scheduler_error;
pub mod scheduler_messages;
pub mod scheduler_metrics;
pub mod thread_aware_account_locks;
pub mod transaction_priority_id;
pub mod transaction_state;
// pub mod scheduler_controller;
pub mod deserializable_packet;
pub mod prio_graph_scheduler;
pub mod transaction_state_container;

#[macro_use]
extern crate solana_metrics;

#[cfg(test)]
#[macro_use]
extern crate assert_matches;

/// Consumer will create chunks of transactions from buffer with up to this size.
pub const TARGET_NUM_TRANSACTIONS_PER_BATCH: usize = 64;

mod read_write_account_set;

#[cfg(test)]
mod tests {
    use {
        crate::deserializable_packet::DeserializableTxPacket,
        solana_compute_budget::compute_budget_limits::ComputeBudgetLimits,
        solana_perf::packet::Packet,
        solana_runtime::bank::Bank,
        solana_runtime_transaction::instructions_processor::process_compute_budget_instructions,
        solana_sanitize::SanitizeError,
        solana_sdk::{
            clock::Slot,
            hash::Hash,
            message::{v0::LoadedAddresses, AddressLoaderError, Message, SimpleAddressLoader},
            pubkey::Pubkey,
            signature::Signature,
            transaction::{
                SanitizedTransaction, SanitizedVersionedTransaction, VersionedTransaction,
            },
        },
        solana_short_vec::decode_shortu16_len,
        solana_svm_transaction::{
            instruction::SVMInstruction, message_address_table_lookup::SVMMessageAddressTableLookup,
        },
        std::{cmp::Ordering, collections::HashSet, mem::size_of},
        thiserror::Error,
    };

    #[derive(Debug, Error)]
    pub enum MockDeserializedPacketError {
        #[error("ShortVec Failed to Deserialize")]
        // short_vec::decode_shortu16_len() currently returns () on error
        ShortVecError(()),
        #[error("Deserialization Error: {0}")]
        DeserializationError(#[from] bincode::Error),
        #[error("overflowed on signature size {0}")]
        SignatureOverflowed(usize),
        #[error("packet failed sanitization {0}")]
        SanitizeError(#[from] SanitizeError),
        #[error("transaction failed prioritization")]
        PrioritizationFailure,
    }

    #[derive(Debug, Eq)]
    pub struct MockImmutableDeserializedPacket {
        pub original_packet: Packet,
        pub transaction: SanitizedVersionedTransaction,
        pub message_hash: Hash,
        pub is_simple_vote: bool,
        pub compute_unit_price: u64,
        pub compute_unit_limit: u32,
    }

    impl DeserializableTxPacket for MockImmutableDeserializedPacket {
        type DeserializeError = MockDeserializedPacketError;
        fn new(packet: Packet) -> Result<Self, Self::DeserializeError> {
            let versioned_transaction: VersionedTransaction = packet.deserialize_slice(..)?;
            let sanitized_transaction =
                SanitizedVersionedTransaction::try_from(versioned_transaction)?;
            let message_bytes = packet_message(&packet)?;
            let message_hash = Message::hash_raw_message(message_bytes);
            let is_simple_vote = packet.meta().is_simple_vote_tx();

            // drop transaction if prioritization fails.
            let ComputeBudgetLimits {
                mut compute_unit_price,
                compute_unit_limit,
                ..
            } = process_compute_budget_instructions(
                sanitized_transaction
                    .get_message()
                    .program_instructions_iter()
                    .map(|(pubkey, ix)| (pubkey, SVMInstruction::from(ix))),
            )
            .map_err(|_| MockDeserializedPacketError::PrioritizationFailure)?;

            // set compute unit price to zero for vote transactions
            if is_simple_vote {
                compute_unit_price = 0;
            };

            Ok(Self {
                original_packet: packet,
                transaction: sanitized_transaction,
                message_hash,
                is_simple_vote,
                compute_unit_price,
                compute_unit_limit,
            })
        }

        fn original_packet(&self) -> &Packet {
            &self.original_packet
        }

        fn transaction(&self) -> &SanitizedVersionedTransaction {
            &self.transaction
        }

        fn message_hash(&self) -> &Hash {
            &self.message_hash
        }

        fn is_simple_vote(&self) -> bool {
            self.is_simple_vote
        }

        fn compute_unit_price(&self) -> u64 {
            self.compute_unit_price
        }

        fn compute_unit_limit(&self) -> u64 {
            u64::from(self.compute_unit_limit)
        }

        // This function deserializes packets into transactions, computes the blake3 hash of transaction
        // messages.
        fn build_sanitized_transaction(
            &self,
            votes_only: bool,
            bank: &Bank,
            reserved_account_keys: &HashSet<Pubkey>,
        ) -> Option<(SanitizedTransaction, Slot)> {
            if votes_only && !self.is_simple_vote() {
                return None;
            }
            // Resolve the lookup addresses and retrieve the min deactivation slot
            let (loaded_addresses, deactivation_slot) =
                Self::resolve_addresses_with_deactivation(self.transaction(), bank).ok()?;
            let address_loader = SimpleAddressLoader::Enabled(loaded_addresses);
            let tx = SanitizedTransaction::try_new(
                self.transaction().clone(),
                *self.message_hash(),
                self.is_simple_vote(),
                address_loader,
                reserved_account_keys,
            )
            .ok()?;
            Some((tx, deactivation_slot))
        }
    }

    impl MockImmutableDeserializedPacket {
        fn resolve_addresses_with_deactivation(
            transaction: &SanitizedVersionedTransaction,
            bank: &Bank,
        ) -> Result<(LoadedAddresses, Slot), AddressLoaderError> {
            let Some(address_table_lookups) =
                transaction.get_message().message.address_table_lookups()
            else {
                return Ok((LoadedAddresses::default(), Slot::MAX));
            };

            bank.load_addresses_from_ref(
                address_table_lookups
                    .iter()
                    .map(SVMMessageAddressTableLookup::from),
            )
        }
    }

    // PartialEq MUST be consistent with PartialOrd and Ord
    impl PartialEq for MockImmutableDeserializedPacket {
        fn eq(&self, other: &Self) -> bool {
            self.compute_unit_price() == other.compute_unit_price()
        }
    }

    impl PartialOrd for MockImmutableDeserializedPacket {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for MockImmutableDeserializedPacket {
        fn cmp(&self, other: &Self) -> Ordering {
            self.compute_unit_price().cmp(&other.compute_unit_price())
        }
    }

    /// Read the transaction message from packet data
    fn packet_message(packet: &Packet) -> Result<&[u8], MockDeserializedPacketError> {
        let (sig_len, sig_size) = packet
            .data(..)
            .and_then(|bytes| decode_shortu16_len(bytes).ok())
            .ok_or(MockDeserializedPacketError::ShortVecError(()))?;
        sig_len
            .checked_mul(size_of::<Signature>())
            .and_then(|v| v.checked_add(sig_size))
            .and_then(|msg_start| packet.data(msg_start..))
            .ok_or(MockDeserializedPacketError::SignatureOverflowed(sig_size))
    }
}
