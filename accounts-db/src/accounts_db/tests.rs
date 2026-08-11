//! Accounts-db test suite.
#![cfg(test)]

use super::*;

mod append_vec;

// re-export these fns that live in impl.rs because ancient append vec tests use them...
pub(crate) use append_vec::r#impl::{
    append_single_account_with_default_hash, compare_all_accounts,
    get_account_from_account_from_storage, get_all_accounts, remove_account_for_tests,
};
