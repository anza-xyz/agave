use {
    super::error::CoreBpfMigrationError,
    solana_core_bpf_migration::callback::AccountLoaderCallback,
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount},
        bpf_loader_upgradeable::{self, get_program_data_address, UpgradeableLoaderState},
        pubkey::Pubkey,
    },
};

/// The account details of a Core BPF program slated to be upgraded.
#[derive(Debug)]
pub(crate) struct TargetCoreBpf {
    pub program_address: Pubkey,
    pub program_data_address: Pubkey,
    pub program_data_account: AccountSharedData,
    pub upgrade_authority_address: Option<Pubkey>,
}

impl TargetCoreBpf {
    /// Collects the details of a Core BPF program and verifies it is properly
    /// configured.
    /// The program account should exist with a pointer to its data account
    /// and it should be marked as executable.
    /// The program data account should exist with the correct state
    /// (a ProgramData header and the program ELF).
    pub(crate) fn new_checked<CB: AccountLoaderCallback>(
        callback: &CB,
        program_address: &Pubkey,
    ) -> Result<Self, CoreBpfMigrationError> {
        let program_data_address = get_program_data_address(program_address);

        // The program account should exist.
        let program_account = callback
            .load_account(program_address)
            .ok_or(CoreBpfMigrationError::AccountNotFound(*program_address))?;

        // The program account should be owned by the upgradeable loader.
        if program_account.owner() != &bpf_loader_upgradeable::id() {
            return Err(CoreBpfMigrationError::IncorrectOwner(*program_address));
        }

        // The program account should be executable.
        if !program_account.executable() {
            return Err(CoreBpfMigrationError::ProgramAccountNotExecutable(
                *program_address,
            ));
        }

        // The program account should have a pointer to its data account.
        match program_account.deserialize_data::<UpgradeableLoaderState>()? {
            UpgradeableLoaderState::Program {
                programdata_address,
            } if programdata_address == program_data_address => (),
            _ => {
                return Err(CoreBpfMigrationError::InvalidProgramAccount(
                    *program_address,
                ))
            }
        }

        // The program data account should exist.
        let program_data_account = callback.load_account(&program_data_address).ok_or(
            CoreBpfMigrationError::ProgramHasNoDataAccount(*program_address),
        )?;

        // The program data account should be owned by the upgradeable loader.
        if program_data_account.owner() != &bpf_loader_upgradeable::id() {
            return Err(CoreBpfMigrationError::IncorrectOwner(program_data_address));
        }

        // The program data account should have the correct state.
        let programdata_metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
        if program_data_account.data().len() >= programdata_metadata_size {
            if let UpgradeableLoaderState::ProgramData {
                upgrade_authority_address,
                ..
            } = bincode::deserialize(&program_data_account.data()[..programdata_metadata_size])?
            {
                return Ok(Self {
                    program_address: *program_address,
                    program_data_address,
                    program_data_account,
                    upgrade_authority_address,
                });
            }
        }
        Err(CoreBpfMigrationError::InvalidProgramDataAccount(
            program_data_address,
        ))
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
                me.store_account(&b.program_id, b.name.as_bytes(), true, &native_loader::id());
            });
            me
        }

        fn store_account(
            &mut self,
            address: &Pubkey,
            data: &[u8],
            executable: bool,
            owner: &Pubkey,
        ) {
            let space = data.len();
            let lamports = self.rent.minimum_balance(space);
            let mut account = AccountSharedData::new(lamports, space, owner);
            account.set_executable(executable);
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
    fn test_target_core_bpf() {
        let mut account_store = SimpleAccountStore::new();

        let program_address = Pubkey::new_unique();
        let program_data_address = get_program_data_address(&program_address);

        // Fail if the program account does not exist.
        assert_matches!(
            TargetCoreBpf::new_checked(&account_store, &program_address).unwrap_err(),
            CoreBpfMigrationError::AccountNotFound(..)
        );

        // Fail if the program account is not owned by the upgradeable loader.
        account_store.store_account(
            &program_address,
            &bincode::serialize(&UpgradeableLoaderState::Program {
                programdata_address: program_data_address,
            })
            .unwrap(),
            true,
            &Pubkey::new_unique(), // Not the upgradeable loader
        );
        assert_matches!(
            TargetCoreBpf::new_checked(&account_store, &program_address).unwrap_err(),
            CoreBpfMigrationError::IncorrectOwner(..)
        );

        // Fail if the program account is not executable.
        account_store.store_account(
            &program_address,
            &bincode::serialize(&UpgradeableLoaderState::Program {
                programdata_address: program_data_address,
            })
            .unwrap(),
            false, // Not executable
            &bpf_loader_upgradeable::id(),
        );
        assert_matches!(
            TargetCoreBpf::new_checked(&account_store, &program_address).unwrap_err(),
            CoreBpfMigrationError::ProgramAccountNotExecutable(..)
        );

        // Fail if the program account does not have the correct state.
        account_store.store_account(
            &program_address,
            &[4u8; 200], // Not the correct state
            true,
            &bpf_loader_upgradeable::id(),
        );
        assert_matches!(
            TargetCoreBpf::new_checked(&account_store, &program_address).unwrap_err(),
            CoreBpfMigrationError::BincodeError(..)
        );

        // Fail if the program account does not have the correct state.
        // This time, valid `UpgradeableLoaderState` but not a program account.
        account_store.store_account(
            &program_address,
            &bincode::serialize(&UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(Pubkey::new_unique()),
            })
            .unwrap(),
            true,
            &bpf_loader_upgradeable::id(),
        );
        assert_matches!(
            TargetCoreBpf::new_checked(&account_store, &program_address).unwrap_err(),
            CoreBpfMigrationError::InvalidProgramAccount(..)
        );

        // Fail if the program account does not have the correct state.
        // This time, valid `UpgradeableLoaderState::Program` but it points to
        // the wrong program data account.
        account_store.store_account(
            &program_address,
            &bincode::serialize(&UpgradeableLoaderState::Program {
                programdata_address: Pubkey::new_unique(), // Not the correct program data account
            })
            .unwrap(),
            true,
            &bpf_loader_upgradeable::id(),
        );

        // Store the proper program account.
        account_store.store_account(
            &program_address,
            &bincode::serialize(&UpgradeableLoaderState::Program {
                programdata_address: program_data_address,
            })
            .unwrap(),
            true,
            &bpf_loader_upgradeable::id(),
        );

        // Fail if the program data account does not exist.
        assert_matches!(
            TargetCoreBpf::new_checked(&account_store, &program_address).unwrap_err(),
            CoreBpfMigrationError::ProgramHasNoDataAccount(..)
        );

        // Fail if the program data account is not owned by the upgradeable loader.
        account_store.store_account(
            &program_data_address,
            &bincode::serialize(&UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(Pubkey::new_unique()),
            })
            .unwrap(),
            false,
            &Pubkey::new_unique(), // Not the upgradeable loader
        );
        assert_matches!(
            TargetCoreBpf::new_checked(&account_store, &program_address).unwrap_err(),
            CoreBpfMigrationError::IncorrectOwner(..)
        );

        // Fail if the program data account does not have the correct state.
        account_store.store_account(
            &program_data_address,
            &[4u8; 200], // Not the correct state
            false,
            &bpf_loader_upgradeable::id(),
        );
        assert_matches!(
            TargetCoreBpf::new_checked(&account_store, &program_address).unwrap_err(),
            CoreBpfMigrationError::BincodeError(..)
        );

        // Fail if the program data account does not have the correct state.
        // This time, valid `UpgradeableLoaderState` but not a program data account.
        account_store.store_account(
            &program_data_address,
            &bincode::serialize(&UpgradeableLoaderState::Program {
                programdata_address: program_data_address,
            })
            .unwrap(),
            false,
            &bpf_loader_upgradeable::id(),
        );
        assert_matches!(
            TargetCoreBpf::new_checked(&account_store, &program_address).unwrap_err(),
            CoreBpfMigrationError::InvalidProgramDataAccount(..)
        );

        // Success
        let elf = vec![4u8; 200];
        let mut test_success = |upgrade_authority_address: Option<Pubkey>| {
            // BPF Loader always writes ELF bytes after
            // `UpgradeableLoaderState::size_of_programdata_metadata()`.
            let programdata_metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
            let data_len = programdata_metadata_size + elf.len();
            let mut data = vec![0u8; data_len];
            bincode::serialize_into(
                &mut data[..programdata_metadata_size],
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address,
                },
            )
            .unwrap();
            data[programdata_metadata_size..].copy_from_slice(&elf);

            account_store.store_account(
                &program_data_address,
                &data,
                false,
                &bpf_loader_upgradeable::id(),
            );

            let target_core_bpf =
                TargetCoreBpf::new_checked(&account_store, &program_address).unwrap();

            assert_eq!(target_core_bpf.program_address, program_address);
            assert_eq!(target_core_bpf.program_data_address, program_data_address);
            assert_eq!(
                bincode::deserialize::<UpgradeableLoaderState>(
                    &target_core_bpf.program_data_account.data()[..programdata_metadata_size]
                )
                .unwrap(),
                UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address
                },
            );
            assert_eq!(
                &target_core_bpf.program_data_account.data()[programdata_metadata_size..],
                elf.as_slice()
            );
        };

        // With authority
        test_success(Some(Pubkey::new_unique()));

        // Without authority
        test_success(None);
    }
}
