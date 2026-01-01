#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![forbid(unsafe_code)]
// Allow deprecated warnings since this crate will be removed along with
// `solana-zk-token-sdk` will be removed
#![allow(deprecated)]

use solana_program_runtime::declare_process_instruction;

declare_process_instruction!(Entrypoint, 0, |_invoke_context| { Ok(()) });
