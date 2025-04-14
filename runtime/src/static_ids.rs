use {
    spl_generic_token::{associated_token_account, token, token_2022},
    solana_pubkey::Pubkey,
};

lazy_static! {
    /// Vector of static token & mint IDs
    pub static ref STATIC_IDS: Vec<Pubkey> = vec![
        associated_token_account::id(),
        token::id(),
        token::native_mint::id(),
        token_2022::id(),
    ];
}
