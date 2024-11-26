use {
    super::error::CoreBpfMigrationError,
    solana_core_bpf_migration::callback::AccountLoaderCallback,
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount},
        bpf_loader_upgradeable::{self, UpgradeableLoaderState},
        pubkey::Pubkey,
    },
};

/// The account details of a buffer account slated to replace a built-in
/// program.
#[derive(Debug)]
pub(crate) struct SourceBuffer {
    pub buffer_address: Pubkey,
    pub buffer_account: AccountSharedData,
}

impl SourceBuffer {
    /// Collects the details of a buffer account and verifies it exists, is
    /// owned by the upgradeable loader, and has the correct state.
    pub(crate) fn new_checked<CB: AccountLoaderCallback>(
        callback: &CB,
        buffer_address: &Pubkey,
    ) -> Result<Self, CoreBpfMigrationError> {
        // The buffer account should exist.
        let buffer_account = callback
            .load_account(buffer_address)
            .ok_or(CoreBpfMigrationError::AccountNotFound(*buffer_address))?;

        // The buffer account should be owned by the upgradeable loader.
        if buffer_account.owner() != &bpf_loader_upgradeable::id() {
            return Err(CoreBpfMigrationError::IncorrectOwner(*buffer_address));
        }

        // The buffer account should have the correct state.
        let buffer_metadata_size = UpgradeableLoaderState::size_of_buffer_metadata();
        if buffer_account.data().len() >= buffer_metadata_size {
            if let UpgradeableLoaderState::Buffer { .. } =
                bincode::deserialize(&buffer_account.data()[..buffer_metadata_size])?
            {
                return Ok(Self {
                    buffer_address: *buffer_address,
                    buffer_account,
                });
            }
        }
        Err(CoreBpfMigrationError::InvalidBufferAccount(*buffer_address))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bank::builtins::BUILTINS,
        assert_matches::assert_matches,
        solana_sdk::{account::WritableAccount, bpf_loader_upgradeable, native_loader, rent::Rent},
        std::collections::HashMap,
    };

    #[derive(Default)]
    struct SimpleAccountStore {
        accounts: HashMap<Pubkey, AccountSharedData>,
        rent: Rent,
    }

    impl SimpleAccountStore {
        fn new() -> Self {
            let mut me = Self::default();
            BUILTINS.iter().for_each(|b| {
                me.store_account(&b.program_id, b.name.as_bytes(), &native_loader::id());
            });
            me
        }

        fn store_account(&mut self, address: &Pubkey, data: &[u8], owner: &Pubkey) {
            let space = data.len();
            let lamports = self.rent.minimum_balance(space);
            let mut account = AccountSharedData::new(lamports, space, owner);
            account.data_as_mut_slice().copy_from_slice(data);
            self.accounts.insert(*address, account);
        }
    }

    impl AccountLoaderCallback for SimpleAccountStore {
        fn load_account(&self, address: &Pubkey) -> Option<AccountSharedData> {
            self.accounts.get(address).cloned()
        }
    }

    #[test]
    fn test_source_buffer() {
        let mut account_store = SimpleAccountStore::new();

        let buffer_address = Pubkey::new_unique();

        // Fail if the buffer account does not exist
        assert_matches!(
            SourceBuffer::new_checked(&account_store, &buffer_address).unwrap_err(),
            CoreBpfMigrationError::AccountNotFound(..)
        );

        // Fail if the buffer account is not owned by the upgradeable loader.
        account_store.store_account(
            &buffer_address,
            &[4u8; 200],
            &Pubkey::new_unique(), // Not the upgradeable loader
        );
        assert_matches!(
            SourceBuffer::new_checked(&account_store, &buffer_address).unwrap_err(),
            CoreBpfMigrationError::IncorrectOwner(..)
        );

        // Fail if the buffer account does not have the correct state.
        account_store.store_account(
            &buffer_address,
            &[4u8; 200], // Not the correct state
            &bpf_loader_upgradeable::id(),
        );
        assert_matches!(
            SourceBuffer::new_checked(&account_store, &buffer_address).unwrap_err(),
            CoreBpfMigrationError::BincodeError(..)
        );

        // Fail if the buffer account does not have the correct state.
        // This time, valid `UpgradeableLoaderState` but not a buffer account.
        account_store.store_account(
            &buffer_address,
            &bincode::serialize(&UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: None,
            })
            .unwrap(),
            &bpf_loader_upgradeable::id(),
        );
        assert_matches!(
            SourceBuffer::new_checked(&account_store, &buffer_address).unwrap_err(),
            CoreBpfMigrationError::InvalidBufferAccount(..)
        );

        // Success
        let elf = vec![4u8; 200];
        let mut test_success = |authority_address: Option<Pubkey>| {
            // BPF Loader always writes ELF bytes after
            // `UpgradeableLoaderState::size_of_buffer_metadata()`.
            let buffer_metadata_size = UpgradeableLoaderState::size_of_buffer_metadata();
            let data_len = buffer_metadata_size + elf.len();
            let mut data = vec![0u8; data_len];
            bincode::serialize_into(
                &mut data[..buffer_metadata_size],
                &UpgradeableLoaderState::Buffer { authority_address },
            )
            .unwrap();
            data[buffer_metadata_size..].copy_from_slice(&elf);

            account_store.store_account(&buffer_address, &data, &bpf_loader_upgradeable::id());

            let source_buffer = SourceBuffer::new_checked(&account_store, &buffer_address).unwrap();

            assert_eq!(source_buffer.buffer_address, buffer_address);
            assert_eq!(
                bincode::deserialize::<UpgradeableLoaderState>(
                    &source_buffer.buffer_account.data()[..buffer_metadata_size]
                )
                .unwrap(),
                UpgradeableLoaderState::Buffer { authority_address },
            );
            assert_eq!(
                &source_buffer.buffer_account.data()[buffer_metadata_size..],
                elf.as_slice()
            );
        };

        // With authority
        test_success(Some(Pubkey::new_unique()));

        // Without authority
        test_success(None);
    }
}
