use {
    crate::{
        callback::AccountLoaderCallback, config::CoreBpfMigrationTargetType,
        error::CoreBpfMigrationError,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_program::bpf_loader_upgradeable::get_program_data_address,
    solana_pubkey::Pubkey,
    solana_sdk_ids::native_loader::ID as NATIVE_LOADER_ID,
};

/// The account details of a built-in program to be migrated to Core BPF.
#[derive(Debug)]
pub struct TargetBuiltin {
    pub program_address: Pubkey,
    pub program_account: AccountSharedData,
    pub program_data_address: Pubkey,
}

impl TargetBuiltin {
    /// Collects the details of a built-in program and verifies it is properly
    /// configured
    pub fn new_checked<CB: AccountLoaderCallback>(
        callback: &CB,
        program_address: &Pubkey,
        migration_target: &CoreBpfMigrationTargetType,
    ) -> Result<Self, CoreBpfMigrationError> {
        let program_account = match migration_target {
            CoreBpfMigrationTargetType::Builtin => {
                // The program account should exist.
                let program_account = callback
                    .load_account(program_address)
                    .ok_or(CoreBpfMigrationError::AccountNotFound(*program_address))?;

                // The program account should be owned by the native loader.
                if program_account.owner() != &NATIVE_LOADER_ID {
                    return Err(CoreBpfMigrationError::IncorrectOwner(*program_address));
                }

                program_account
            }
            CoreBpfMigrationTargetType::Stateless => {
                // The program account should _not_ exist.
                if callback.load_account(program_address).is_some() {
                    return Err(CoreBpfMigrationError::AccountExists(*program_address));
                }

                AccountSharedData::default()
            }
        };

        let program_data_address = get_program_data_address(program_address);

        // The program data account should not exist.
        if callback.load_account(&program_data_address).is_some() {
            return Err(CoreBpfMigrationError::ProgramHasDataAccount(
                *program_address,
            ));
        }

        Ok(Self {
            program_address: *program_address,
            program_account,
            program_data_address,
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::prototypes::BUILTINS,
        assert_matches::assert_matches,
        solana_account::Account,
        solana_program::bpf_loader_upgradeable::UpgradeableLoaderState,
        solana_rent::Rent,
        solana_sdk_ids::{bpf_loader_upgradeable::ID as BPF_LOADER_UPGRADEABLE_ID, native_loader},
        std::collections::HashMap,
        test_case::test_case,
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

        fn store_account<T: serde::Serialize + ?Sized>(
            &mut self,
            address: &Pubkey,
            data: &T,
            executable: bool,
            owner: &Pubkey,
        ) {
            let data = bincode::serialize(data).unwrap();
            let data_len = data.len();
            let lamports = self.rent.minimum_balance(data_len);
            let account = AccountSharedData::from(Account {
                data,
                executable,
                lamports,
                owner: *owner,
                ..Account::default()
            });
            self.accounts.insert(*address, account);
        }

        fn clear_account(&mut self, address: &Pubkey) {
            self.accounts.remove(address);
        }
    }

    impl AccountLoaderCallback for SimpleAccountStore {
        fn load_account(&self, address: &Pubkey) -> Option<AccountSharedData> {
            self.accounts.get(address).cloned()
        }
    }

    #[test_case(solana_sdk_ids::address_lookup_table::id())]
    #[test_case(solana_sdk_ids::bpf_loader::id())]
    #[test_case(solana_sdk_ids::bpf_loader_deprecated::id())]
    #[test_case(solana_sdk_ids::bpf_loader_upgradeable::id())]
    #[test_case(solana_sdk_ids::compute_budget::id())]
    #[test_case(solana_sdk_ids::config::id())]
    #[test_case(solana_sdk_ids::loader_v4::id())]
    #[test_case(solana_sdk_ids::stake::id())]
    #[test_case(solana_sdk_ids::system_program::id())]
    #[test_case(solana_sdk_ids::vote::id())]
    #[test_case(solana_sdk_ids::zk_elgamal_proof_program::id())]
    #[test_case(solana_sdk_ids::zk_token_proof_program::id())]
    fn test_target_program_builtin(program_address: Pubkey) {
        let migration_target = CoreBpfMigrationTargetType::Builtin;
        let mut account_store = SimpleAccountStore::new();

        let program_account = account_store.load_account(&program_address).unwrap();
        let program_data_address = get_program_data_address(&program_address);

        // Success
        let target_builtin =
            TargetBuiltin::new_checked(&account_store, &program_address, &migration_target)
                .unwrap();
        assert_eq!(target_builtin.program_address, program_address);
        assert_eq!(target_builtin.program_account, program_account);
        assert_eq!(target_builtin.program_data_address, program_data_address);

        // Fail if the program account is not owned by the native loader
        account_store.store_account(
            &program_address,
            &String::from("some built-in program"),
            true,
            &Pubkey::new_unique(), // Not the native loader
        );
        assert_matches!(
            TargetBuiltin::new_checked(&account_store, &program_address, &migration_target)
                .unwrap_err(),
            CoreBpfMigrationError::IncorrectOwner(..)
        );

        // Fail if the program data account exists
        account_store.store_account(
            &program_address,
            &program_account.data(),
            program_account.executable(),
            program_account.owner(),
        );
        account_store.store_account(
            &program_data_address,
            &UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(Pubkey::new_unique()),
            },
            false,
            &BPF_LOADER_UPGRADEABLE_ID,
        );
        assert_matches!(
            TargetBuiltin::new_checked(&account_store, &program_address, &migration_target)
                .unwrap_err(),
            CoreBpfMigrationError::ProgramHasDataAccount(..)
        );

        // Fail if the program account does not exist
        account_store.clear_account(&program_address);
        assert_matches!(
            TargetBuiltin::new_checked(&account_store, &program_address, &migration_target)
                .unwrap_err(),
            CoreBpfMigrationError::AccountNotFound(..)
        );
    }

    #[test_case(solana_sdk_ids::feature::id())]
    #[test_case(solana_sdk_ids::native_loader::id())]
    fn test_target_program_stateless_builtin(program_address: Pubkey) {
        let migration_target = CoreBpfMigrationTargetType::Stateless;
        let mut account_store = SimpleAccountStore::new();

        let program_account = AccountSharedData::default();
        let program_data_address = get_program_data_address(&program_address);

        // Success
        let target_builtin =
            TargetBuiltin::new_checked(&account_store, &program_address, &migration_target)
                .unwrap();
        assert_eq!(target_builtin.program_address, program_address);
        assert_eq!(target_builtin.program_account, program_account);
        assert_eq!(target_builtin.program_data_address, program_data_address);

        // Fail if the program data account exists
        account_store.store_account(
            &program_data_address,
            &UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(Pubkey::new_unique()),
            },
            false,
            &BPF_LOADER_UPGRADEABLE_ID,
        );
        assert_matches!(
            TargetBuiltin::new_checked(&account_store, &program_address, &migration_target)
                .unwrap_err(),
            CoreBpfMigrationError::ProgramHasDataAccount(..)
        );

        // Fail if the program account exists
        account_store.store_account(
            &program_address,
            &String::from("some built-in program"),
            true,
            &NATIVE_LOADER_ID,
        );
        assert_matches!(
            TargetBuiltin::new_checked(&account_store, &program_address, &migration_target)
                .unwrap_err(),
            CoreBpfMigrationError::AccountExists(..)
        );
    }
}
