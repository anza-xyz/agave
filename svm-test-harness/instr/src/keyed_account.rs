//! Keyed account helpers.

use {
    solana_account::Account,
    solana_builtins::BUILTINS,
    solana_loader_v4_interface::state::{LoaderV4State, LoaderV4Status},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
};

fn create_keyed_account_for_builtin_program(program_id: &Pubkey, name: &str) -> (Pubkey, Account) {
    let data = name.as_bytes().to_vec();
    let lamports = Rent::default().minimum_balance(data.len());
    let account = Account {
        lamports,
        data,
        owner: solana_sdk_ids::native_loader::id(),
        executable: true,
        ..Default::default()
    };
    (*program_id, account)
}

pub fn keyed_account_for_system_program() -> (Pubkey, Account) {
    create_keyed_account_for_builtin_program(&BUILTINS[0].program_id, BUILTINS[0].name)
}

pub fn keyed_account_for_compute_budget_program() -> (Pubkey, Account) {
    create_keyed_account_for_builtin_program(&BUILTINS[5].program_id, BUILTINS[5].name)
}

pub fn keyed_account_for_loader_v4_program() -> (Pubkey, Account) {
    create_keyed_account_for_builtin_program(&BUILTINS[7].program_id, BUILTINS[7].name)
}

pub fn create_program_account_loader_v4(
    slot: u64,
    authority_address_or_next_version: Pubkey,
    status: LoaderV4Status,
    elf: &[u8],
) -> Account {
    let data = unsafe {
        let elf_offset = LoaderV4State::program_data_offset();
        let data_len = elf_offset.saturating_add(elf.len());
        let mut data = vec![0u8; data_len];
        *std::mem::transmute::<&mut [u8; LoaderV4State::program_data_offset()], &mut LoaderV4State>(
            (&mut data[0..elf_offset]).try_into().unwrap(),
        ) = LoaderV4State {
            slot,
            authority_address_or_next_version,
            status,
        };
        data[elf_offset..].copy_from_slice(elf);
        data
    };
    let lamports = Rent::default().minimum_balance(data.len());
    Account {
        lamports,
        data,
        owner: solana_sdk_ids::loader_v4::id(),
        executable: true,
        ..Default::default()
    }
}
