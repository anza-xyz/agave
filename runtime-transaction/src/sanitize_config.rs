//! Canonical [`SanitizeConfig`] construction for agave.
//!
//! `agave-transaction-view` does not depend on agave crates, so the protocol
//! limits enforced during sanitization are supplied by the caller. This module
//! is the single place where those limits are sourced from the agave constants.

use {
    agave_transaction_view::sanitize::SanitizeConfig,
    solana_compute_budget::compute_budget_limits::{MAX_HEAP_FRAME_BYTES, MIN_HEAP_FRAME_BYTES},
    solana_transaction_context::{MAX_ACCOUNTS_PER_INSTRUCTION, MAX_INSTRUCTION_TRACE_LENGTH},
};

/// Returns the [`SanitizeConfig`] with current protocol limits.
pub fn sanitize_config() -> SanitizeConfig {
    SanitizeConfig {
        min_requested_heap_size: MIN_HEAP_FRAME_BYTES,
        max_requested_heap_size: MAX_HEAP_FRAME_BYTES,
        max_instructions: MAX_INSTRUCTION_TRACE_LENGTH,
        max_accounts_per_instruction: MAX_ACCOUNTS_PER_INSTRUCTION,
    }
}
