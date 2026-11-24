use {
    common::{LoaderV3Features, process_instruction_with_conformance_harness},
    solana_account::{Account, state_traits::StateMutWincode as StateMut},
    solana_instruction::{AccountMeta, Instruction},
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_program_runtime::execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
};

mod common;

#[test]
fn test_bpf_loader_upgradeable_initialize_buffer() {
    let loader_id = bpf_loader_upgradeable::id();
    let buffer_address = Pubkey::new_unique();
    let buffer_account = Account::new_with_data(
        1,
        vec![0; UpgradeableLoaderState::size_of_buffer(9)],
        &loader_id,
    );
    let authority_address = Pubkey::new_unique();
    let authority_account = Account::new_with_data(
        1,
        vec![0; UpgradeableLoaderState::size_of_buffer(9)],
        &loader_id,
    );
    let instruction_data =
        wincode::serialize(&UpgradeableLoaderInstruction::InitializeBuffer).unwrap();
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
    let accounts = process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(loader_id, &instruction_data, instruction_accounts.clone()),
        vec![
            (buffer_address, buffer_account),
            (authority_address, authority_account),
        ],
        LoaderV3Features::all_enabled(),
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        Ok(()),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().1.state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address)
        }
    );

    // Case: Already initialized
    let accounts = process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(loader_id, &instruction_data, instruction_accounts),
        vec![
            (buffer_address, accounts.first().unwrap().1.clone()),
            (authority_address, accounts.get(1).unwrap().1.clone()),
        ],
        LoaderV3Features::all_enabled(),
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        Err(InstructionError::AccountAlreadyInitialized),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().1.state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address)
        }
    );
}
