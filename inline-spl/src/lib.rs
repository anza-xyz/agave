use solana_pubkey::Pubkey;

pub mod associated_token_account;
pub mod token;
pub mod token_2022;

// Returns all known SPL Token program ids
pub fn spl_token_ids() -> Vec<Pubkey> {
    vec![token::id(), token_2022::id()]
}

// Check if the provided program id as a known SPL Token program id
pub fn is_known_spl_token_id(program_id: &Pubkey) -> bool {
    *program_id == token::id() || *program_id == token_2022::id()
}
