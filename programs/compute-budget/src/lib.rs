#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
use solana_program_runtime::declare_process_instruction;

pub const DEFAULT_COMPUTE_UNITS: u64 = 150;

declare_process_instruction!(Entrypoint, DEFAULT_COMPUTE_UNITS, |_invoke_context| {
    // Do nothing, compute budget instructions handled by the runtime
    Ok(())
});
