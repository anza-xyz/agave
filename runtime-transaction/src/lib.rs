#![cfg_attr(RUSTC_WITH_SPECIALIZATION, feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]

mod builtin_auxiliary_data_store;
mod instruction_details;
pub mod instructions_processor;
pub mod runtime_transaction;
pub mod transaction_meta;
