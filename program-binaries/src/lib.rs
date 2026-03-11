#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]

use {
    solana_account::{Account, AccountSharedData, ReadableAccount},
    solana_loader_v3_interface::{get_program_data_address, state::UpgradeableLoaderState},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable},
};

mod spl_memo_1_0 {
    solana_pubkey::declare_id!("Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo");
}
mod spl_memo_3_0 {
    solana_pubkey::declare_id!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
}

static SPL_PROGRAMS: &[(Pubkey, Pubkey, &[u8])] = &[
    (
        spl_generic_token::token::ID,
        solana_sdk_ids::bpf_loader::ID,
        include_bytes!("programs/spl_token-3.5.0.so"),
    ),
    (
        spl_generic_token::token_2022::ID,
        solana_sdk_ids::bpf_loader_upgradeable::ID,
        include_bytes!("programs/spl_token_2022-10.0.0.so"),
    ),
    (
        spl_memo_1_0::ID,
        solana_sdk_ids::bpf_loader::ID,
        include_bytes!("programs/spl_memo-1.0.0.so"),
    ),
    (
        spl_memo_3_0::ID,
        solana_sdk_ids::bpf_loader::ID,
        include_bytes!("programs/spl_memo-3.0.0.so"),
    ),
    (
        spl_generic_token::associated_token_account::ID,
        solana_sdk_ids::bpf_loader::ID,
        include_bytes!("programs/spl_associated_token_account-1.1.1.so"),
    ),
];

// Programs that were previously builtins but have been migrated to Core BPF.
// All Core BPF programs are owned by BPF loader v3.
// Note the second pubkey is the migration feature ID. A `None` value denotes
// activation on all clusters, therefore no feature gate.
static CORE_BPF_PROGRAMS: &[(Pubkey, Option<Pubkey>, &[u8])] = &[
    (
        solana_sdk_ids::address_lookup_table::ID,
        None,
        include_bytes!("programs/core_bpf_address_lookup_table-3.0.0.so"),
    ),
    (
        solana_sdk_ids::config::ID,
        None,
        include_bytes!("programs/core_bpf_config-3.0.0.so"),
    ),
    (
        solana_sdk_ids::feature::ID,
        None,
        include_bytes!("programs/core_bpf_feature_gate-0.0.1.so"),
    ),
    (
        solana_sdk_ids::stake::ID,
        None,
        include_bytes!("programs/core_bpf_stake-1.0.1.so"),
    ),
    // Add more programs here post-migration...
];

/// Returns a tuple `(Pubkey, Account)` for a BPF program, where the key is the
/// provided program ID and the account is a valid BPF Loader program account
/// containing the ELF.
fn bpf_loader_program_account(program_id: &Pubkey, elf: &[u8], rent: &Rent) -> (Pubkey, Account) {
    (
        *program_id,
        Account {
            lamports: rent.minimum_balance(elf.len()).max(1),
            data: elf.to_vec(),
            owner: bpf_loader::id(),
            executable: true,
            rent_epoch: u64::MAX,
        },
    )
}

/// Returns two tuples `(Pubkey, Account)` for a BPF upgradeable program.
/// The first tuple is the program account. It contains the provided program ID
/// and an account with a pointer to its program data account.
/// The second tuple is the program data account. It contains the program data
/// address and an account with the program data - a valid BPF Loader Upgradeable
/// program data account containing the ELF.
pub fn bpf_loader_upgradeable_program_accounts(
    program_id: &Pubkey,
    elf: &[u8],
    rent: &Rent,
) -> [(Pubkey, Account); 2] {
    let programdata_address = get_program_data_address(program_id);
    let program_account = {
        let space = UpgradeableLoaderState::size_of_program();
        let lamports = rent.minimum_balance(space);
        let data = bincode::serialize(&UpgradeableLoaderState::Program {
            programdata_address,
        })
        .unwrap();
        Account {
            lamports,
            data,
            owner: bpf_loader_upgradeable::id(),
            executable: true,
            rent_epoch: u64::MAX,
        }
    };
    let programdata_account = {
        let space = UpgradeableLoaderState::size_of_programdata_metadata() + elf.len();
        let lamports = rent.minimum_balance(space);
        let mut data = bincode::serialize(&UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(Pubkey::default()),
        })
        .unwrap();
        data.extend_from_slice(elf);
        Account {
            lamports,
            data,
            owner: bpf_loader_upgradeable::id(),
            executable: false,
            rent_epoch: u64::MAX,
        }
    };
    [
        (*program_id, program_account),
        (programdata_address, programdata_account),
    ]
}

pub fn spl_programs(rent: &Rent) -> Vec<(Pubkey, AccountSharedData)> {
    SPL_PROGRAMS
        .iter()
        .flat_map(|(program_id, loader_id, elf)| {
            let mut accounts = vec![];
            if loader_id.eq(&solana_sdk_ids::bpf_loader_upgradeable::ID) {
                for (key, account) in bpf_loader_upgradeable_program_accounts(program_id, elf, rent)
                {
                    accounts.push((key, AccountSharedData::from(account)));
                }
            } else {
                let (key, account) = bpf_loader_program_account(program_id, elf, rent);
                accounts.push((key, AccountSharedData::from(account)));
            }
            accounts
        })
        .collect()
}

pub fn core_bpf_programs<F>(rent: &Rent, is_feature_active: F) -> Vec<(Pubkey, AccountSharedData)>
where
    F: Fn(&Pubkey) -> bool,
{
    CORE_BPF_PROGRAMS
        .iter()
        .flat_map(|(program_id, feature_id, elf)| {
            let mut accounts = vec![];
            if feature_id.is_none() || feature_id.is_some_and(|f| is_feature_active(&f)) {
                for (key, account) in bpf_loader_upgradeable_program_accounts(program_id, elf, rent)
                {
                    accounts.push((key, AccountSharedData::from(account)));
                }
            }
            accounts
        })
        .collect()
}

pub fn by_id(program_id: &Pubkey, rent: &Rent) -> Option<Vec<(Pubkey, AccountSharedData)>> {
    let programs = spl_programs(rent);
    if let Some(i) = programs.iter().position(|(key, _)| key == program_id) {
        let n = num_accounts(programs[i].1.owner());
        return Some(programs.into_iter().skip(i).take(n).collect());
    }

    let programs = core_bpf_programs(rent, |_| true);
    if let Some(i) = programs.iter().position(|(key, _)| key == program_id) {
        let n = num_accounts(programs[i].1.owner());
        return Some(programs.into_iter().skip(i).take(n).collect());
    }

    None
}

fn num_accounts(owner_id: &Pubkey) -> usize {
    if *owner_id == bpf_loader_upgradeable::id() {
        2
    } else {
        1
    }
}
