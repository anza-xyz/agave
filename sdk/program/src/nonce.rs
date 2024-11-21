#[deprecated(since = "2.1.0", note = "Use `solana-nonce` crate instead")]
pub use solana_nonce::{state::State, NONCED_TX_MARKER_IX_INDEX};
pub mod state {
    #[deprecated(since = "2.1.0", note = "Use `solana-nonce` crate instead")]
    pub use solana_nonce::{
        state::{Data, DurableNonce, State},
        versions::{AuthorizeNonceError, Versions},
    };
}
