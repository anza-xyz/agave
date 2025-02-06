//! Example Rust-based SBF noop program

use {
    solana_account_info::AccountInfo,
    solana_program_error::ProgramResult,
    solana_pubkey::Pubkey
};

solana_package_metadata::declare_id_with_package_metadata!("solana.program-id");
solana_program_entrypoint::entrypoint!(process_instruction);
fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    Ok(())
}
