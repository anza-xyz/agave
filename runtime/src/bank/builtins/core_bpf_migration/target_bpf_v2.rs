#![allow(dead_code)]
use {
    super::error::CoreBpfMigrationError,
    crate::bank::Bank,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_loader_v3_interface::get_program_data_address,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, system_program},
};

/// The account details of a Loader v2 BPF program slated to be upgraded.
#[derive(Debug)]
pub(crate) struct TargetBpfV2 {
    pub program_address: Pubkey,
    pub program_account: AccountSharedData,
    pub program_data_address: Pubkey,
    pub program_data_account_lamports: u64,
}

impl TargetBpfV2 {
    /// Collects the details of a Loader v2 BPF program and verifies it is properly
    /// configured.
    ///
    /// The program account should exist and it should be marked as executable.
    pub(crate) fn new_checked(
        bank: &Bank,
        program_address: &Pubkey,
    ) -> Result<Self, CoreBpfMigrationError> {
        // The program account should exist.
        let program_account = bank
            .get_account_with_fixed_root(program_address)
            .ok_or(CoreBpfMigrationError::AccountNotFound(*program_address))?;

        // The program account should be owned by the loader v2.
        if program_account.owner() != &bpf_loader::id() {
            return Err(CoreBpfMigrationError::IncorrectOwner(*program_address));
        }

        // The program account should be executable.
        if !program_account.executable() {
            return Err(CoreBpfMigrationError::ProgramAccountNotExecutable(
                *program_address,
            ));
        }

        let program_data_address = get_program_data_address(program_address);

        // The program data account is expected not to exist.
        let program_data_account_lamports =
            if let Some(account) = bank.get_account_with_fixed_root(&program_data_address) {
                // The program data account should not exist, but a system account with funded
                // lamports is acceptable.
                if account.owner() != &system_program::id() {
                    return Err(CoreBpfMigrationError::ProgramHasDataAccount(
                        *program_address,
                    ));
                }
                account.lamports()
            } else {
                0
            };

        Ok(Self {
            program_address: *program_address,
            program_account,
            program_data_address,
            program_data_account_lamports,
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::bank::tests::create_simple_test_bank, assert_matches::assert_matches,
        solana_account::WritableAccount, solana_sdk_ids::bpf_loader,
    };

    fn store_account(bank: &Bank, address: &Pubkey, data: &[u8], owner: &Pubkey, executable: bool) {
        let space = data.len();
        let lamports = bank.get_minimum_balance_for_rent_exemption(space);
        let mut account = AccountSharedData::new(lamports, space, owner);
        account.set_executable(executable);
        account.data_as_mut_slice().copy_from_slice(data);
        bank.store_account_and_update_capitalization(address, &account);
    }

    #[test]
    fn test_target_bpf_v2() {
        let bank = create_simple_test_bank(0);

        let program_address = Pubkey::new_unique();
        let elf = vec![4u8; 200];

        // Fail if the program account does not exist.
        assert_matches!(
            TargetBpfV2::new_checked(&bank, &program_address).unwrap_err(),
            CoreBpfMigrationError::AccountNotFound(..)
        );

        // Fail if the program account is not owned by the loader v2.
        store_account(
            &bank,
            &program_address,
            &elf,
            &Pubkey::new_unique(), // Not the loader v2
            true,
        );
        assert_matches!(
            TargetBpfV2::new_checked(&bank, &program_address).unwrap_err(),
            CoreBpfMigrationError::IncorrectOwner(..)
        );

        // Fail if the program account is not executable.
        store_account(
            &bank,
            &program_address,
            &elf,
            &bpf_loader::id(),
            false, // Not executable
        );
        assert_matches!(
            TargetBpfV2::new_checked(&bank, &program_address).unwrap_err(),
            CoreBpfMigrationError::ProgramAccountNotExecutable(..)
        );

        // Success
        store_account(&bank, &program_address, &elf, &bpf_loader::id(), true);

        let target_bpf_v2 = TargetBpfV2::new_checked(&bank, &program_address).unwrap();

        assert_eq!(target_bpf_v2.program_address, program_address);
        assert_eq!(target_bpf_v2.program_account.data(), elf.as_slice());
    }
}
