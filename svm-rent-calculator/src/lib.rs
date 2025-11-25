#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable \
        API. From v4.0.0 onward, the `agave-unstable-api` crate feature must be specified \
        to acknowledge use of an interface that may break without warning."
))]
//! Solana SVM Rent Calculator.
//!
//! Rent management for SVM.

pub mod rent_state;
pub mod svm_rent_calculator;
