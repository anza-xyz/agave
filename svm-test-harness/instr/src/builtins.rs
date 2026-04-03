//! SVM-resident builtin programs for the test harness.

use {
    solana_program_runtime::{
        invoke_context::BuiltinFunctionRegisterer, solana_sbpf::program::BuiltinFunctionDefinition,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable},
};

pub struct SvmBuiltinPrototype {
    pub program_id: Pubkey,
    pub name: &'static str,
    pub register_fn: BuiltinFunctionRegisterer,
}

pub static SVM_BUILTINS: &[SvmBuiltinPrototype] = &[
    SvmBuiltinPrototype {
        program_id: solana_system_program::id(),
        name: "system_program",
        register_fn: solana_system_program::system_processor::Entrypoint::register,
    },
    SvmBuiltinPrototype {
        program_id: bpf_loader_deprecated::id(),
        name: "solana_bpf_loader_deprecated_program",
        register_fn: solana_bpf_loader_program::Entrypoint::register,
    },
    SvmBuiltinPrototype {
        program_id: bpf_loader::id(),
        name: "solana_bpf_loader_program",
        register_fn: solana_bpf_loader_program::Entrypoint::register,
    },
    SvmBuiltinPrototype {
        program_id: bpf_loader_upgradeable::id(),
        name: "solana_bpf_loader_upgradeable_program",
        register_fn: solana_bpf_loader_program::Entrypoint::register,
    },
    SvmBuiltinPrototype {
        program_id: solana_sdk_ids::compute_budget::id(),
        name: "compute_budget_program",
        register_fn: solana_compute_budget_program::Entrypoint::register,
    },
];
