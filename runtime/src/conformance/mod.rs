//! Solana runtime conformance harnesses.

#[cfg(feature = "conformance")]
pub mod cost;
pub mod txn;

#[cfg(feature = "conformance")]
use {
    protosol::protos::TransactionMessage as ProtoTransactionMessage,
    solana_hash::Hash,
    solana_message::{
        MessageHeader, VersionedMessage,
        compiled_instruction::CompiledInstruction,
        legacy,
        v0::{self, MessageAddressTableLookup},
    },
    solana_pubkey::Pubkey,
};

#[cfg(feature = "conformance")]
pub(crate) fn versioned_message_from_proto(value: &ProtoTransactionMessage) -> VersionedMessage {
    let header = value
        .header
        .map(|header| MessageHeader {
            // A valid message has at least one signature.
            // Truncate to the u8 header field *before* clamping to >= 1 (matching
            // protosol): a u32 that is a nonzero multiple of 256 truncates to 0, and
            // must still clamp up to 1 rather than staying 0 (no fee payer).
            num_required_signatures: (header.num_required_signatures as u8).max(1),
            num_readonly_signed_accounts: header.num_readonly_signed_accounts as u8,
            num_readonly_unsigned_accounts: header.num_readonly_unsigned_accounts as u8,
        })
        .unwrap_or(MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        });
    let account_keys = value
        .account_keys
        .iter()
        .filter_map(|key| Pubkey::try_from(key.as_slice()).ok())
        .collect::<Vec<_>>();
    let recent_blockhash = <[u8; 32]>::try_from(value.recent_blockhash.as_slice())
        .map(Hash::new_from_array)
        .unwrap_or_default();
    let instructions = value
        .instructions
        .iter()
        .map(|instruction| CompiledInstruction {
            program_id_index: instruction.program_id_index as u8,
            accounts: instruction
                .accounts
                .iter()
                .map(|index| *index as u8)
                .collect(),
            data: instruction.data.clone(),
        })
        .collect::<Vec<_>>();

    if value.is_legacy {
        VersionedMessage::Legacy(legacy::Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
        })
    } else {
        let address_table_lookups = value
            .address_table_lookups
            .iter()
            .filter_map(|lookup| {
                Pubkey::try_from(lookup.account_key.as_slice())
                    .ok()
                    .map(|account_key| MessageAddressTableLookup {
                        account_key,
                        writable_indexes: lookup
                            .writable_indexes
                            .iter()
                            .map(|index| *index as u8)
                            .collect(),
                        readonly_indexes: lookup
                            .readonly_indexes
                            .iter()
                            .map(|index| *index as u8)
                            .collect(),
                    })
            })
            .collect::<Vec<_>>();
        VersionedMessage::V0(v0::Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
            address_table_lookups,
        })
    }
}
