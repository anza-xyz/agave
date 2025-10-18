#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![allow(clippy::arithmetic_side_effects)]

mod builtin_programs_filter;
pub mod compute_budget_instruction_details;
mod compute_budget_program_id_filter;
pub mod instructions_processor;
