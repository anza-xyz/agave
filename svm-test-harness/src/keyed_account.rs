//! Keyed account helpers.

use {
    solana_account::Account, solana_builtins::BUILTINS, solana_pubkey::Pubkey, solana_rent::Rent,
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
