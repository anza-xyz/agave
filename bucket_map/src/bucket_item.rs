#![cfg(feature = "dev-context-only-utils")]
use solana_pubkey::Pubkey;

#[derive(Debug, Default, Clone)]
pub struct BucketItem<T> {
    pub pubkey: Pubkey,
    pub slot_list: Vec<T>,
}
