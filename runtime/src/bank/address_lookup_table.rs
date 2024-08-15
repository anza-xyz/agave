use {
    super::Bank,
    agave_transaction_view::address_table_lookup_meta::MessageAddressTableLookupRef,
    solana_sdk::{
        address_lookup_table::error::AddressLookupError,
        message::{
            v0::{LoadedAddresses, MessageAddressTableLookup},
            AddressLoaderError,
        },
        transaction::AddressLoader,
    },
};

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

impl AddressLoader for &Bank {
    fn load_addresses(
        self,
        address_table_lookups: &[MessageAddressTableLookup],
    ) -> Result<LoadedAddresses, AddressLoaderError> {
        self.load_addresses_from_ref(
            address_table_lookups
                .iter()
                .map(MessageAddressTableLookupRef::from),
        )
    }
}

impl Bank {
    /// Load addresses from an iterator of `MessageAddressTableLookupRef`.
    pub fn load_addresses_from_ref<'a>(
        &self,
        address_table_lookups: impl Iterator<Item = MessageAddressTableLookupRef<'a>>,
    ) -> Result<LoadedAddresses, AddressLoaderError> {
        let slot_hashes = self
            .transaction_processor
            .sysvar_cache()
            .get_slot_hashes()
            .map_err(|_| AddressLoaderError::SlotHashesSysvarNotFound)?;

        address_table_lookups
            .map(|address_table_lookup| {
                self.rc
                    .accounts
                    .load_lookup_table_addresses(
                        &self.ancestors,
                        address_table_lookup,
                        &slot_hashes,
                    )
                    .map_err(into_address_loader_error)
            })
            .collect::<Result<_, _>>()
    }
}
