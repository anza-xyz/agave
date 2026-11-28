use {
    solana_builtins_default_costs::{
        BuiltinMigrationFeatureIndex, MAYBE_BUILTIN_KEY, MIGRATING_BUILTINS_COSTS,
        get_builtin_migration_feature_index,
    },
    solana_pubkey::Pubkey,
};

// The maximum number of pubkeys that a packet can contain.
pub(crate) const FILTER_SIZE: usize = u8::MAX as usize + 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ProgramKind {
    NotBuiltin,
    Builtin,
    // Builtin program maybe in process of being migrated to core bpf,
    // if core_bpf_migration_feature is activated, then the migration has
    // completed and it should no longer be considered as builtin
    MigratingBuiltin {
        core_bpf_migration_feature_index: usize,
    },
}

// A ProgramKind stored in one byte, or UNCHECKED. Plain bytes make `new()` a
// single 256-byte zero fill; it runs for every transaction that doesn't set a
// compute unit limit.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EncodedProgramKind(u8);

impl EncodedProgramKind {
    const UNCHECKED: Self = Self(0);
    const NOT_BUILTIN: Self = Self(1);
    const BUILTIN: Self = Self(2);
    const FIRST_MIGRATING: u8 = 3;

    #[inline]
    fn decode(self) -> Option<ProgramKind> {
        match self {
            Self::UNCHECKED => None,
            Self::NOT_BUILTIN => Some(ProgramKind::NotBuiltin),
            Self::BUILTIN => Some(ProgramKind::Builtin),
            Self(code) => Some(ProgramKind::MigratingBuiltin {
                core_bpf_migration_feature_index: usize::from(code - Self::FIRST_MIGRATING),
            }),
        }
    }
}

const _: () = assert!(
    MIGRATING_BUILTINS_COSTS.len() <= (u8::MAX - EncodedProgramKind::FIRST_MIGRATING) as usize + 1,
    "every migrating builtin index must fit in an EncodedProgramKind"
);

impl From<ProgramKind> for EncodedProgramKind {
    #[inline]
    fn from(kind: ProgramKind) -> Self {
        match kind {
            ProgramKind::NotBuiltin => Self::NOT_BUILTIN,
            ProgramKind::Builtin => Self::BUILTIN,
            // Fits in a byte: checked by the const assertion above.
            ProgramKind::MigratingBuiltin {
                core_bpf_migration_feature_index,
            } => Self(Self::FIRST_MIGRATING + core_bpf_migration_feature_index as u8),
        }
    }
}

pub(crate) struct BuiltinProgramsFilter {
    // array of slots for all possible static and sanitized program_id_index,
    // each slot indicates if a program_id_index has not been checked (UNCHECKED),
    // or already checked with an encoded ProgramKind that can be reused.
    encoded_program_kinds: [EncodedProgramKind; FILTER_SIZE],
}

impl BuiltinProgramsFilter {
    pub(crate) fn new() -> Self {
        BuiltinProgramsFilter {
            encoded_program_kinds: [EncodedProgramKind::UNCHECKED; FILTER_SIZE],
        }
    }

    pub(crate) fn get_program_kind(&mut self, index: usize, program_id: &Pubkey) -> ProgramKind {
        let slot = self
            .encoded_program_kinds
            .get_mut(index)
            .expect("program id index is sanitized");
        slot.decode().unwrap_or_else(|| {
            let kind = Self::check_program_kind(program_id);
            *slot = kind.into();
            kind
        })
    }

    #[inline]
    fn check_program_kind(program_id: &Pubkey) -> ProgramKind {
        if !MAYBE_BUILTIN_KEY[program_id.as_ref()[0] as usize] {
            return ProgramKind::NotBuiltin;
        }

        match get_builtin_migration_feature_index(program_id) {
            BuiltinMigrationFeatureIndex::NotBuiltin => ProgramKind::NotBuiltin,
            BuiltinMigrationFeatureIndex::BuiltinNoMigrationFeature => ProgramKind::Builtin,
            BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature(
                core_bpf_migration_feature_index,
            ) => ProgramKind::MigratingBuiltin {
                core_bpf_migration_feature_index,
            },
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_builtins_default_costs::{
            BuiltinCost, MIGRATING_BUILTINS_COSTS, MigratingBuiltinCost, get_migration_feature_id,
        },
    };

    const DUMMY_PROGRAM_ID: &str = "dummmy1111111111111111111111111111111111111";

    #[test]
    fn get_program_kind() {
        let mut test_store = BuiltinProgramsFilter::new();
        let mut index = 9;

        // initial state is Unchecked
        assert_eq!(
            test_store.encoded_program_kinds[index],
            EncodedProgramKind::UNCHECKED
        );

        // non builtin returns None
        assert_eq!(
            test_store.get_program_kind(index, &DUMMY_PROGRAM_ID.parse().unwrap()),
            ProgramKind::NotBuiltin
        );
        // but its state is now checked
        assert_eq!(
            test_store.encoded_program_kinds[index],
            EncodedProgramKind::NOT_BUILTIN
        );
        // lookup same `index` will return cached data, will not lookup `program_id`
        // again
        assert_eq!(
            test_store.get_program_kind(index, &solana_sdk_ids::bpf_loader_upgradeable::id()),
            ProgramKind::NotBuiltin
        );

        // not-migrating builtin
        index += 1;
        assert_eq!(
            test_store.get_program_kind(index, &solana_sdk_ids::bpf_loader_upgradeable::id()),
            ProgramKind::Builtin,
        );

        // compute-budget
        index += 1;
        assert_eq!(
            test_store.get_program_kind(index, &solana_sdk_ids::compute_budget::id()),
            ProgramKind::Builtin,
        );

        // migrating builtins
        for (program_id, migrating_builtin) in MIGRATING_BUILTINS_COSTS {
            index += 1;

            let BuiltinCost::Migrating(MigratingBuiltinCost {
                core_bpf_migration_feature,
                position,
            }) = migrating_builtin
            else {
                panic!("MIGRATING_BUILTINS_COSTS must only contain BuiltinCost::Migrating");
            };

            assert_eq!(
                get_migration_feature_id(*position),
                core_bpf_migration_feature
            );

            assert_eq!(
                test_store.get_program_kind(index, program_id),
                ProgramKind::MigratingBuiltin {
                    core_bpf_migration_feature_index: *position,
                }
            );
        }
    }

    #[test]
    #[should_panic(expected = "program id index is sanitized")]
    fn test_get_program_kind_out_of_bound_index() {
        let mut test_store = BuiltinProgramsFilter::new();
        assert_eq!(
            test_store.get_program_kind(FILTER_SIZE + 1, &DUMMY_PROGRAM_ID.parse().unwrap(),),
            ProgramKind::NotBuiltin
        );
    }

    #[test]
    fn test_encoded_program_kind_round_trip() {
        assert_eq!(EncodedProgramKind::UNCHECKED.decode(), None);

        let kinds =
            [ProgramKind::NotBuiltin, ProgramKind::Builtin]
                .into_iter()
                .chain((0..MIGRATING_BUILTINS_COSTS.len()).map(
                    |core_bpf_migration_feature_index| ProgramKind::MigratingBuiltin {
                        core_bpf_migration_feature_index,
                    },
                ));
        for kind in kinds {
            let encoded = EncodedProgramKind::from(kind);
            assert_ne!(encoded, EncodedProgramKind::UNCHECKED);
            assert_eq!(encoded.decode(), Some(kind));
        }
    }
}
