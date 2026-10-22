use {
    solana_pubkey::Pubkey,
    spl_generic_token::{associated_token_account, token, token_2022},
};

/// Vector of static token & mint IDs
pub static STATIC_IDS: std::sync::LazyLock<Vec<Pubkey>> = std::sync::LazyLock::new(|| {
    vec![
        associated_token_account::id(),
        token::id(),
        token::native_mint::id(),
        token_2022::id(),
    ]
});
