//! Transaction address lookup table loader.

use {
    solana_account::{Account, ReadableAccount},
    solana_address_lookup_table_interface::{error::AddressLookupError, state::AddressLookupTable},
    solana_clock::Slot,
    solana_message::{
        AddressLoader,
        v0::{LoadedAddresses, MessageAddressTableLookup},
    },
    solana_pubkey::Pubkey,
    solana_slot_hashes::SlotHashes,
    solana_transaction_error::AddressLoaderError,
};

#[derive(Clone, Copy)]
pub struct TransactionAddressLoader<'a> {
    pub accounts: &'a [(Pubkey, Account)],
    pub slot: Slot,
    pub slot_hashes: &'a SlotHashes,
}

impl AddressLoader for TransactionAddressLoader<'_> {
    fn load_addresses(
        self,
        lookups: &[MessageAddressTableLookup],
    ) -> Result<LoadedAddresses, AddressLoaderError> {
        let mut loaded_addresses = LoadedAddresses::default();

        for lookup in lookups {
            let table_account = self
                .accounts
                .iter()
                .find(|(key, account)| key == &lookup.account_key && account.lamports() > 0)
                .map(|(_, account)| account)
                .ok_or(AddressLoaderError::LookupTableAccountNotFound)?;

            if !solana_address_lookup_table_interface::program::check_id(table_account.owner()) {
                return Err(AddressLoaderError::InvalidAccountOwner);
            }

            let lookup_table = AddressLookupTable::deserialize(table_account.data())
                .map_err(|_| AddressLoaderError::InvalidAccountData)?;
            loaded_addresses.writable.extend(
                lookup_table
                    .lookup_iter(self.slot, &lookup.writable_indexes, self.slot_hashes)
                    .map_err(into_address_loader_error)?
                    .collect::<Option<Vec<_>>>()
                    .ok_or(AddressLoaderError::InvalidLookupIndex)?,
            );
            loaded_addresses.readonly.extend(
                lookup_table
                    .lookup_iter(self.slot, &lookup.readonly_indexes, self.slot_hashes)
                    .map_err(into_address_loader_error)?
                    .collect::<Option<Vec<_>>>()
                    .ok_or(AddressLoaderError::InvalidLookupIndex)?,
            );
        }

        Ok(loaded_addresses)
    }
}

fn into_address_loader_error(err: AddressLookupError) -> AddressLoaderError {
    match err {
        AddressLookupError::LookupTableAccountNotFound => {
            AddressLoaderError::LookupTableAccountNotFound
        }
        AddressLookupError::InvalidAccountOwner => AddressLoaderError::InvalidAccountOwner,
        AddressLookupError::InvalidAccountData => AddressLoaderError::InvalidAccountData,
        AddressLookupError::InvalidLookupIndex => AddressLoaderError::InvalidLookupIndex,
    }
}
