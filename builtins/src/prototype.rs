//! Prototype layouts for builtins.

use {
    crate::core_bpf_migration::CoreBpfMigrationConfig,
    solana_program_runtime::invoke_context::BuiltinFunctionWithContext, solana_pubkey::Pubkey,
};

/// Transitions of built-in programs at epoch boundaries when features are activated.
pub struct BuiltinPrototype {
    /// Configurations for migrating the builtin to Core BPF.
    pub core_bpf_migration_config: Option<CoreBpfMigrationConfig>,
    /// Feature ID that enables the builtin program.
    /// If None, the built-in program is always enabled.
    pub enable_feature_id: Option<Pubkey>,
    /// The program's ID.
    pub program_id: Pubkey,
    /// The program's name, ie "system_program".
    pub name: &'static str,
    /// The program's entrypoint function.
    pub entrypoint: BuiltinFunctionWithContext,
}

impl std::fmt::Debug for BuiltinPrototype {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut builder = f.debug_struct("BuiltinPrototype");
        builder.field("program_id", &self.program_id);
        builder.field("name", &self.name);
        builder.field("enable_feature_id", &self.enable_feature_id);
        builder.field("core_bpf_migration_config", &self.core_bpf_migration_config);
        builder.finish()
    }
}

/// Transitions of stateless built-in programs at epoch boundaries when
/// features are activated.
/// These are built-in programs that don't actually exist, but their address
/// is reserved.
#[derive(Debug)]
pub struct StatelessBuiltinPrototype {
    /// Configurations for migrating the stateless builtin to Core BPF.
    pub core_bpf_migration_config: Option<CoreBpfMigrationConfig>,
    /// The program's ID.
    pub program_id: Pubkey,
    /// The program's name, ie "feature_gate_program".
    pub name: &'static str,
}
