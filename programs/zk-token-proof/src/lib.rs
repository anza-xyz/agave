#![cfg(feature = "agave-unstable-api")]
#![forbid(unsafe_code)]

use solana_program_runtime::declare_process_instruction;

declare_process_instruction!(Entrypoint, 0, |_invoke_context| { Ok(()) });
