//! Transaction message utilities.

#![cfg(feature = "fuzz")]

use {
    crate::fixture::proto::{
        CompiledInstruction as ProtoCompiledInstruction,
        MessageAddressTableLookup as ProtoMessageAddressTableLookup,
        MessageHeader as ProtoMessageHeader, TransactionMessage as ProtoTransactionMessage,
    },
    solana_hash::Hash,
    solana_message::{
        compiled_instruction::CompiledInstruction,
        legacy,
        v0::{self, MessageAddressTableLookup},
        MessageHeader, VersionedMessage,
    },
    solana_pubkey::Pubkey,
    std::cmp::max,
};

impl From<&ProtoMessageHeader> for MessageHeader {
    fn from(value: &ProtoMessageHeader) -> Self {
        MessageHeader {
            num_required_signatures: max(1, value.num_required_signatures as u8),
            num_readonly_signed_accounts: value.num_readonly_signed_accounts as u8,
            num_readonly_unsigned_accounts: value.num_readonly_unsigned_accounts as u8,
        }
    }
}

impl From<&ProtoCompiledInstruction> for CompiledInstruction {
    fn from(value: &ProtoCompiledInstruction) -> Self {
        CompiledInstruction {
            program_id_index: value.program_id_index as u8,
            accounts: value.accounts.iter().map(|idx| *idx as u8).collect(),
            data: value.data.clone(),
        }
    }
}

impl From<&ProtoMessageAddressTableLookup> for MessageAddressTableLookup {
    fn from(value: &ProtoMessageAddressTableLookup) -> Self {
        MessageAddressTableLookup {
            account_key: Pubkey::new_from_array(value.account_key.clone().try_into().unwrap()),
            writable_indexes: value
                .writable_indexes
                .iter()
                .map(|idx| *idx as u8)
                .collect(),
            readonly_indexes: value
                .readonly_indexes
                .iter()
                .map(|idx| *idx as u8)
                .collect(),
        }
    }
}

pub fn build_versioned_message(value: &ProtoTransactionMessage) -> VersionedMessage {
    let header = if let Some(value_header) = value.header {
        MessageHeader::from(&value_header)
    } else {
        MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        }
    };
    let account_keys = value
        .account_keys
        .iter()
        .map(|key| Pubkey::new_from_array(key.clone().try_into().unwrap()))
        .collect::<Vec<Pubkey>>();
    let recent_blockhash = if value.recent_blockhash.is_empty() {
        Hash::new_from_array([0u8; 32])
    } else {
        Hash::new_from_array(value.recent_blockhash.clone().try_into().unwrap())
    };
    let instructions = value
        .instructions
        .iter()
        .map(CompiledInstruction::from)
        .collect::<Vec<CompiledInstruction>>();

    if value.is_legacy {
        let message = legacy::Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
        };
        VersionedMessage::Legacy(message)
    } else {
        let address_table_lookups = value
            .address_table_lookups
            .iter()
            .map(MessageAddressTableLookup::from)
            .collect::<Vec<MessageAddressTableLookup>>();

        let message = v0::Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
            address_table_lookups,
        };

        VersionedMessage::V0(message)
    }
}
