use {
    agave_transaction_view::static_account_keys_frame::MAX_STATIC_ACCOUNTS_PER_PACKET as FILTER_SIZE,
    solana_builtins_default_costs::{get_builtin_migration_feature_id, MAYBE_BUILTIN_KEY},
    solana_sdk::pubkey::Pubkey,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ProgramKind {
    NotBuiltin,
    Builtin,
    // Builtin program maybe in process of being migrated to core bpf,
    // if core_bpf_migration_feature is activated, then the migration has
    // completed and it should not longer be considered as builtin
    MigratingBuiltin {
        core_bpf_migration_feature_id: Pubkey,
    },
}

pub(crate) struct BuiltinProgramIdFilter {
    // array of slots for all possible static and sanitized program_id_index,
    // each slot indicates if a program_id_index has not been checked (eg, None),
    // or already checked with result (eg, Some(ProgramKind)) that can be reused.
    program_kind: [Option<ProgramKind>; FILTER_SIZE as usize],
}

impl BuiltinProgramIdFilter {
    pub(crate) fn new() -> Self {
        BuiltinProgramIdFilter {
            program_kind: [None; FILTER_SIZE as usize],
        }
    }

    #[inline]
    pub(crate) fn get_program_kind(&mut self, index: usize, program_id: &Pubkey) -> ProgramKind {
        *self
            .program_kind
            .get_mut(index)
            .expect("program id index is sanitized")
            .get_or_insert_with(|| Self::check_program_kind(program_id))
    }

    #[inline]
    fn check_program_kind(program_id: &Pubkey) -> ProgramKind {
        if !MAYBE_BUILTIN_KEY[program_id.as_ref()[0] as usize] {
            return ProgramKind::NotBuiltin;
        }

        get_builtin_migration_feature_id(program_id).map_or(
            ProgramKind::NotBuiltin,
            |core_bpf_migration_feature_id| match core_bpf_migration_feature_id {
                Some(core_bpf_migration_feature_id) => ProgramKind::MigratingBuiltin {
                    core_bpf_migration_feature_id,
                },
                None => ProgramKind::Builtin,
            },
        )
    }
}
