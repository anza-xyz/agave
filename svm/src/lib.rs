#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(missing_unsafe_on_extern)]
#![warn(rust_2024_guarded_string_incompatible_syntax)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

pub mod account_loader;
pub mod account_overrides;
pub mod message_processor;
pub mod nonce_info;
pub mod program_loader;
pub mod rollback_accounts;
pub mod transaction_account_state_info;
pub mod transaction_balances;
pub mod transaction_commit_result;
pub mod transaction_error_metrics;
pub mod transaction_execution_result;
pub mod transaction_processing_callback;
pub mod transaction_processing_result;
pub mod transaction_processor;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;
