use {
    common::{LoaderV3Features, process_instruction, process_instruction_with_setup},
    rand::Rng,
    solana_account::{AccountSharedData, WritableAccount},
    solana_bpf_loader_program::test_utils,
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated},
    std::{fs::File, io::Read, ops::Range},
};

mod common;

fn load_program_account_from_elf(loader_id: &Pubkey, path: &str) -> AccountSharedData {
    let mut file = File::open(path).expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let rent = Rent::default();
    let mut program_account = AccountSharedData::new(rent.minimum_balance(elf.len()), 0, loader_id);
    program_account.set_data_from_slice(&elf);
    program_account.set_executable(true);
    program_account
}
#[test]
fn test_bpf_loader_invoke_main() {
    let loader_id = bpf_loader::id();
    let program_id = Pubkey::new_unique();
    let program_account =
        load_program_account_from_elf(&loader_id, "test_elfs/out/sbpfv3_return_ok.so");
    let parameter_id = Pubkey::new_unique();
    let parameter_account = AccountSharedData::new(1, 0, &loader_id);
    let parameter_meta = AccountMeta {
        pubkey: parameter_id,
        is_signer: false,
        is_writable: false,
    };

    // Case: No program account
    process_instruction(
        &loader_id,
        &[],
        Vec::new(),
        Vec::new(),
        Err(InstructionError::UnsupportedProgramId),
    );

    // Case: Only a program account
    process_instruction(
        &program_id,
        &[],
        vec![(program_id, program_account.clone())],
        Vec::new(),
        Ok(()),
    );

    // Case: With program and parameter account
    process_instruction(
        &program_id,
        &[],
        vec![
            (program_id, program_account.clone()),
            (parameter_id, parameter_account.clone()),
        ],
        vec![parameter_meta.clone()],
        Ok(()),
    );

    // Case: With duplicate accounts
    process_instruction(
        &program_id,
        &[],
        vec![
            (program_id, program_account.clone()),
            (parameter_id, parameter_account.clone()),
        ],
        vec![parameter_meta.clone(), parameter_meta],
        Ok(()),
    );

    // Case: limited budget
    process_instruction_with_setup(
        &program_id,
        &[],
        vec![(program_id, program_account)],
        Vec::new(),
        LoaderV3Features::all_enabled(),
        Err(InstructionError::ProgramFailedToComplete),
        |invoke_context| {
            invoke_context.mock_set_remaining(0);
            test_utils::load_all_invoked_programs(invoke_context);
        },
    );

    // Case: Account not a program
    process_instruction_with_setup(
        &program_id,
        &[],
        vec![(program_id, parameter_account.clone())],
        Vec::new(),
        LoaderV3Features::all_enabled(),
        Err(InstructionError::UnsupportedProgramId),
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
    );
    process_instruction(
        &program_id,
        &[],
        vec![(program_id, parameter_account)],
        Vec::new(),
        Err(InstructionError::UnsupportedProgramId),
    );
}
#[test]
fn test_bpf_loader_serialize_unaligned() {
    let loader_id = bpf_loader_deprecated::id();
    let program_id = Pubkey::new_unique();
    let program_account =
        load_program_account_from_elf(&loader_id, "test_elfs/out/noop_unaligned.so");
    let parameter_id = Pubkey::new_unique();
    let parameter_account = AccountSharedData::new(1, 0, &loader_id);
    let parameter_meta = AccountMeta {
        pubkey: parameter_id,
        is_signer: false,
        is_writable: false,
    };

    // Case: With program and parameter account
    process_instruction(
        &program_id,
        &[],
        vec![
            (program_id, program_account.clone()),
            (parameter_id, parameter_account.clone()),
        ],
        vec![parameter_meta.clone()],
        Ok(()),
    );

    // Case: With duplicate accounts
    process_instruction(
        &program_id,
        &[],
        vec![
            (program_id, program_account),
            (parameter_id, parameter_account),
        ],
        vec![parameter_meta.clone(), parameter_meta],
        Ok(()),
    );
}
#[test]
fn test_bpf_loader_serialize_aligned() {
    let loader_id = bpf_loader::id();
    let program_id = Pubkey::new_unique();
    let program_account =
        load_program_account_from_elf(&loader_id, "test_elfs/out/noop_aligned.so");
    let parameter_id = Pubkey::new_unique();
    let parameter_account = AccountSharedData::new(1, 0, &loader_id);
    let parameter_meta = AccountMeta {
        pubkey: parameter_id,
        is_signer: false,
        is_writable: false,
    };

    // Case: With program and parameter account
    process_instruction(
        &program_id,
        &[],
        vec![
            (program_id, program_account.clone()),
            (parameter_id, parameter_account.clone()),
        ],
        vec![parameter_meta.clone()],
        Ok(()),
    );

    // Case: With duplicate accounts
    process_instruction(
        &program_id,
        &[],
        vec![
            (program_id, program_account),
            (parameter_id, parameter_account),
        ],
        vec![parameter_meta.clone(), parameter_meta],
        Ok(()),
    );
}
/// fuzzing utility function
fn fuzz<F>(
    bytes: &[u8],
    outer_iters: usize,
    inner_iters: usize,
    offset: Range<usize>,
    value: Range<u8>,
    work: F,
) where
    F: Fn(&mut [u8]),
{
    let mut rng = rand::rng();
    for _ in 0..outer_iters {
        let mut mangled_bytes = bytes.to_vec();
        for _ in 0..inner_iters {
            let offset = rng.random_range(offset.start..offset.end);
            let value = rng.random_range(value.start..value.end);
            *mangled_bytes.get_mut(offset).unwrap() = value;
            work(&mut mangled_bytes);
        }
    }
}
#[test]
#[ignore]
fn test_fuzz() {
    let loader_id = bpf_loader::id();
    let program_id = Pubkey::new_unique();

    // Create program account
    let mut file = File::open("test_elfs/out/sbpfv3_return_ok.so").expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();

    // Mangle the whole file
    fuzz(
        &elf,
        1_000_000_000,
        100,
        0..elf.len(),
        0..255,
        |bytes: &mut [u8]| {
            let mut program_account = AccountSharedData::new(1, 0, &loader_id);
            program_account.set_data_from_slice(bytes);
            program_account.set_executable(true);
            process_instruction(
                &program_id,
                &[],
                vec![(program_id, program_account)],
                Vec::new(),
                Ok(()),
            );
        },
    );
}
