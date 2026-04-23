//! Invokes the BPF upgradeable loader's `Close` instruction via CPI.
//!
//! Accounts (in order) passed to this program:
//!   0. programdata (writable)
//!   1. recipient (writable)
//!   2. authority (signer)
//!   3. program (writable)
//!   4. bpf_loader_upgradeable program (readonly; needed for the CPI to load
//!      its target invoker)
//!
//! The single byte of instruction data encodes the `tombstone` flag (non-zero
//! means `true`). See `programs/sbf/tests/programs.rs` for the companion test.

use {
    solana_account_info::AccountInfo,
    solana_loader_v3_interface::instruction as loader_v3_instruction,
    solana_program::program::invoke, solana_program_error::ProgramResult, solana_pubkey::Pubkey,
};

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let tombstone = instruction_data.first().copied().unwrap_or(0) != 0;
    let ix = loader_v3_instruction::close_any(
        accounts[0].key,
        accounts[1].key,
        Some(accounts[2].key),
        Some(accounts[3].key),
        tombstone,
    );
    invoke(&ix, accounts)
}
