use {
    common::process_instruction,
    solana_account::{AccountSharedData, state_traits::StateMutWincode as StateMut},
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
};

mod common;

#[test]
fn test_bpf_loader_upgradeable_initialize_buffer() {
    let loader_id = bpf_loader_upgradeable::id();
    let buffer_address = Pubkey::new_unique();
    let buffer_account =
        AccountSharedData::new(1, UpgradeableLoaderState::size_of_buffer(9), &loader_id);
    let authority_address = Pubkey::new_unique();
    let authority_account =
        AccountSharedData::new(1, UpgradeableLoaderState::size_of_buffer(9), &loader_id);
    let instruction_data =
        bincode::serialize(&UpgradeableLoaderInstruction::InitializeBuffer).unwrap();
    let instruction_accounts = vec![
        AccountMeta {
            pubkey: buffer_address,
            is_signer: false,
            is_writable: true,
        },
        AccountMeta {
            pubkey: authority_address,
            is_signer: false,
            is_writable: false,
        },
    ];

    // Case: Success
    let accounts = process_instruction(
        &loader_id,
        &instruction_data,
        vec![
            (buffer_address, buffer_account),
            (authority_address, authority_account),
        ],
        instruction_accounts.clone(),
        Ok(()),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address)
        }
    );

    // Case: Already initialized
    let accounts = process_instruction(
        &loader_id,
        &instruction_data,
        vec![
            (buffer_address, accounts.first().unwrap().clone()),
            (authority_address, accounts.get(1).unwrap().clone()),
        ],
        instruction_accounts,
        Err(InstructionError::AccountAlreadyInitialized),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address)
        }
    );
}
