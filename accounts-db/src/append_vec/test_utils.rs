//! Helpers for AppendVec tests and benches
#![cfg(feature = "dev-context-only-utils")]
use {
    rand::{distr::Alphanumeric, Rng},
    solana_account::AccountSharedData,
    solana_pubkey::Pubkey,
    std::path::PathBuf,
};

pub struct TempFile {
    pub path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let path = std::mem::replace(&mut self.path, PathBuf::new());
        let _ignored = std::fs::remove_file(path);
    }
}

pub fn get_append_vec_dir() -> String {
    std::env::var("FARF_DIR").unwrap_or_else(|_| "farf/append_vec_tests".to_string())
}

pub fn get_append_vec_path(path: &str) -> TempFile {
    let out_dir = get_append_vec_dir();
    let rand_string: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .map(char::from)
        .take(30)
        .collect();
    let dir = format!("{out_dir}/{rand_string}");
    let mut buf = PathBuf::new();
    buf.push(format!("{dir}/{path}"));
    std::fs::create_dir_all(dir).expect("Create directory failed");
    TempFile { path: buf }
}

/// return a test account.
/// Note that `sample`=0 returns a fully default account with a default pubkey.
pub fn create_test_account(sample: usize) -> (Pubkey, AccountSharedData) {
    let data_len = sample % 256;
    let mut account = AccountSharedData::new(sample as u64, 0, &Pubkey::default());
    account.set_data_from_slice(&vec![data_len as u8; data_len]);
    (Pubkey::default(), account)
}

/// Create a test account for the given `data_len`.
/// This is useful to create very large test account.
pub fn create_test_account_with(data_len: usize) -> (Pubkey, AccountSharedData) {
    let account = AccountSharedData::new(100, data_len, &Pubkey::default());
    (Pubkey::default(), account)
}
