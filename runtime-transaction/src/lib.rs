#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]

mod builtin_auxiliary_data_store;
mod instruction_details;
pub mod instructions_processor;
pub mod runtime_transaction;
pub mod signature_details;
pub mod transaction_meta;
