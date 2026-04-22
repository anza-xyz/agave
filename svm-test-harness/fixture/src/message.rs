//! Transaction message utilities.

#![cfg(feature = "fuzz")]

use {
    super::{
        error::FixtureError,
        proto::{
            CompiledInstruction as ProtoCompiledInstruction,
            MessageAddressTableLookup as ProtoMessageAddressTableLookup,
            MessageHeader as ProtoMessageHeader, TransactionMessage as ProtoTransactionMessage,
        },
    },
    solana_account::Account,
    solana_address_lookup_table_interface::state::AddressLookupTable,
    solana_hash::Hash,
    solana_message::{
        LegacyMessage, MessageHeader, SanitizedMessage,
        compiled_instruction::CompiledInstruction,
        legacy,
        v0::{self, LoadedAddresses, LoadedMessage, MessageAddressTableLookup},
    },
    solana_pubkey::Pubkey,
    std::{cmp::max, collections::HashSet},
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

impl TryFrom<&ProtoMessageAddressTableLookup> for MessageAddressTableLookup {
    type Error = FixtureError;

    fn try_from(value: &ProtoMessageAddressTableLookup) -> Result<Self, Self::Error> {
        let account_key = Pubkey::new_from_array(
            value
                .account_key
                .clone()
                .try_into()
                .map_err(FixtureError::InvalidPubkeyBytes)?,
        );
        Ok(MessageAddressTableLookup {
            account_key,
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
        })
    }
}

fn resolve_address_table_lookups(
    lookups: &[MessageAddressTableLookup],
    accounts: &[(Pubkey, Account)],
) -> Result<LoadedAddresses, FixtureError> {
    let mut writable = Vec::new();
    let mut readonly = Vec::new();

    for lookup in lookups {
        let alt_account = accounts
            .iter()
            .find(|(pubkey, _)| *pubkey == lookup.account_key)
            .map(|(_, account)| account)
            .ok_or(FixtureError::InvalidAddressLookupTable)?;

        let alt = AddressLookupTable::deserialize(&alt_account.data)
            .map_err(|_| FixtureError::InvalidAddressLookupTable)?;

        for &idx in &lookup.writable_indexes {
            writable.push(
                *alt.addresses
                    .get(idx as usize)
                    .ok_or(FixtureError::InvalidAddressLookupTable)?,
            );
        }
        for &idx in &lookup.readonly_indexes {
            readonly.push(
                *alt.addresses
                    .get(idx as usize)
                    .ok_or(FixtureError::InvalidAddressLookupTable)?,
            );
        }
    }

    Ok(LoadedAddresses { writable, readonly })
}

pub fn build_sanitized_message(
    value: &ProtoTransactionMessage,
    accounts: &[(Pubkey, Account)],
) -> Result<SanitizedMessage, FixtureError> {
    let header = value
        .header
        .as_ref()
        .map(MessageHeader::from)
        .ok_or(FixtureError::InvalidFixtureInput)?;

    let account_keys = value
        .account_keys
        .iter()
        .map(|key| {
            Ok(Pubkey::new_from_array(
                key.clone()
                    .try_into()
                    .map_err(FixtureError::InvalidPubkeyBytes)?,
            ))
        })
        .collect::<Result<Vec<Pubkey>, FixtureError>>()?;

    let recent_blockhash = if value.recent_blockhash.is_empty() {
        Hash::new_from_array([0u8; 32])
    } else {
        Hash::new_from_array(
            value
                .recent_blockhash
                .clone()
                .try_into()
                .map_err(FixtureError::InvalidHashBytes)?,
        )
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
        Ok(SanitizedMessage::Legacy(LegacyMessage::new(
            message,
            &HashSet::new(),
        )))
    } else {
        let address_table_lookups = value
            .address_table_lookups
            .iter()
            .map(MessageAddressTableLookup::try_from)
            .collect::<Result<Vec<MessageAddressTableLookup>, FixtureError>>()?;

        let loaded_addresses = resolve_address_table_lookups(&address_table_lookups, accounts)?;

        let message = v0::Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
            address_table_lookups,
        };

        Ok(SanitizedMessage::V0(LoadedMessage::new(
            message,
            loaded_addresses,
            &HashSet::new(),
        )))
    }
}
