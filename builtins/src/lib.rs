#![cfg(feature = "agave-unstable-api")]
//! Solana builtin programs.
//!
//! Warning: This crate is not for public consumption. It will change, and
//! could possibly be removed altogether in the future. For now, it is purely
//! for the purpose of managing the migration of builtins to Core BPF.
//!
//! It serves as a source of truth for:
//! * The list of builtins that a Bank should add.
//! * Which of those builtins have been assigned a feature gate to migrate to
//!   Core BPF, as well as whether or not that feature gate has been activated.

pub mod core_bpf_migration;
pub mod prototype;

use {
    crate::{
        core_bpf_migration::{CoreBpfMigrationConfig, CoreBpfMigrationTargetType},
        prototype::{BuiltinPrototype, StatelessBuiltinPrototype},
    },
    agave_feature_set as feature_set,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable},
};

macro_rules! testable_prototype {
    ($prototype:ident {
        core_bpf_migration_config: $core_bpf_migration_config:expr,
        name: $name:ident,
        $($field:ident : $value:expr),* $(,)?
    }) => {
        $prototype {
            core_bpf_migration_config: {
                #[cfg(not(feature = "dev-context-only-utils"))]
                {
                    $core_bpf_migration_config
                }
                #[cfg(feature = "dev-context-only-utils")]
                {
                    Some( test_only::$name::CONFIG )
                }
            },
            name: stringify!($name),
            $($field: $value),*
        }
    };
}

/// DEVELOPER: when a builtin is migrated to sbpf, please add its corresponding
/// migration feature ID to solana-builtin-default-costs::BUILTIN_INSTRUCTION_COSTS,
/// so the builtin's default cost can be determined properly based on feature status.
/// When migration completed, and the feature gate is enabled everywhere, please
/// remove that builtin entry from solana-builtin-default-costs::BUILTIN_INSTRUCTION_COSTS.
pub static BUILTINS: &[BuiltinPrototype] = &[
    testable_prototype!(BuiltinPrototype {
        core_bpf_migration_config: None,
        name: system_program,
        enable_feature_id: None,
        program_id: solana_system_program::id(),
        entrypoint: solana_system_program::system_processor::Entrypoint::vm,
    }),
    testable_prototype!(BuiltinPrototype {
        core_bpf_migration_config: None,
        name: vote_program,
        enable_feature_id: None,
        program_id: solana_vote_program::id(),
        entrypoint: solana_vote_program::vote_processor::Entrypoint::vm,
    }),
    testable_prototype!(BuiltinPrototype {
        core_bpf_migration_config: None,
        name: solana_bpf_loader_deprecated_program,
        enable_feature_id: None,
        program_id: bpf_loader_deprecated::id(),
        entrypoint: solana_bpf_loader_program::Entrypoint::vm,
    }),
    testable_prototype!(BuiltinPrototype {
        core_bpf_migration_config: None,
        name: solana_bpf_loader_program,
        enable_feature_id: None,
        program_id: bpf_loader::id(),
        entrypoint: solana_bpf_loader_program::Entrypoint::vm,
    }),
    testable_prototype!(BuiltinPrototype {
        core_bpf_migration_config: None,
        name: solana_bpf_loader_upgradeable_program,
        enable_feature_id: None,
        program_id: bpf_loader_upgradeable::id(),
        entrypoint: solana_bpf_loader_program::Entrypoint::vm,
    }),
    testable_prototype!(BuiltinPrototype {
        core_bpf_migration_config: None,
        name: compute_budget_program,
        enable_feature_id: None,
        program_id: solana_sdk_ids::compute_budget::id(),
        entrypoint: solana_compute_budget_program::Entrypoint::vm,
    }),
    testable_prototype!(BuiltinPrototype {
        core_bpf_migration_config: None,
        name: zk_token_proof_program,
        enable_feature_id: Some(feature_set::zk_token_sdk_enabled::id()),
        program_id: solana_sdk_ids::zk_token_proof_program::id(),
        entrypoint: solana_zk_token_proof_program::Entrypoint::vm,
    }),
    testable_prototype!(BuiltinPrototype {
        core_bpf_migration_config: None,
        name: loader_v4,
        enable_feature_id: Some(feature_set::enable_loader_v4::id()),
        program_id: solana_sdk_ids::loader_v4::id(),
        entrypoint: solana_loader_v4_program::Entrypoint::vm,
    }),
    testable_prototype!(BuiltinPrototype {
        core_bpf_migration_config: None,
        name: zk_elgamal_proof_program,
        enable_feature_id: Some(feature_set::zk_elgamal_proof_program_enabled::id()),
        program_id: solana_sdk_ids::zk_elgamal_proof_program::id(),
        entrypoint: solana_zk_elgamal_proof_program::Entrypoint::vm,
    }),
];

pub static STATELESS_BUILTINS: &[StatelessBuiltinPrototype] = &[StatelessBuiltinPrototype {
    core_bpf_migration_config: Some(CoreBpfMigrationConfig {
        source_buffer_address: buffer_accounts::slashing_program::id(),
        upgrade_authority_address: None,
        feature_id: feature_set::enshrine_slashing_program::id(),
        verified_build_hash: Some(buffer_accounts::slashing_program::VERIFIED_BUILD_HASH),
        migration_target: CoreBpfMigrationTargetType::Stateless,
        datapoint_name: "enshrine_slashing_program",
    }),
    program_id: buffer_accounts::slashing_program::PROGRAM_ID,
    name: "solana_slashing_program",
}];

/// Live source buffer accounts for builtin migrations.
mod buffer_accounts {
    pub mod slashing_program {
        use {solana_hash::Hash, solana_pubkey::Pubkey};

        solana_pubkey::declare_id!("S1asHs4je6wPb2kWiHqNNdpNRiDaBEDQyfyCThhsrgv");

        pub(crate) const PROGRAM_ID: Pubkey =
            Pubkey::from_str_const("S1ashing11111111111111111111111111111111111");
        // 192ed727334abe822d5accba8b886e25f88b03c76973c2e7290cfb55b9e1115f
        const HASH_BYTES: [u8; 32] = [
            0x19, 0x2e, 0xd7, 0x27, 0x33, 0x4a, 0xbe, 0x82, 0x2d, 0x5a, 0xcc, 0xba, 0x8b, 0x88,
            0x6e, 0x25, 0xf8, 0x8b, 0x03, 0xc7, 0x69, 0x73, 0xc2, 0xe7, 0x29, 0x0c, 0xfb, 0x55,
            0xb9, 0xe1, 0x11, 0x5f,
        ];
        pub(crate) const VERIFIED_BUILD_HASH: Hash = Hash::new_from_array(HASH_BYTES);
    }
}

// This module contains a number of arbitrary addresses used for testing Core
// BPF migrations.
// Since the list of builtins is static, using `declare_id!` with constant
// values is arguably the least-overhead approach to injecting static addresses
// into the builtins list for both the feature ID and the source program ID.
// These arbitrary IDs can then be used to configure feature-activation runtime
// tests.
#[cfg(any(test, feature = "dev-context-only-utils"))]
pub mod test_only {
    use crate::core_bpf_migration::{CoreBpfMigrationConfig, CoreBpfMigrationTargetType};
    pub mod system_program {
        pub mod feature {
            solana_pubkey::declare_id!("AnjsdWg7LXFbjDdy78wncCJs9PyTdWpKkFmHAwQU1mQ6");
        }
        pub mod source_buffer {
            solana_pubkey::declare_id!("EDEhzg1Jk79Wrk4mwpRa7txjgRxcE6igXwd6egFDVhuz");
        }
        pub mod upgrade_authority {
            solana_pubkey::declare_id!("4d14UK2o1FKKoecEBWhVDZrBBbRuhug75G1j9XYCawC2");
        }
        pub const CONFIG: super::CoreBpfMigrationConfig = super::CoreBpfMigrationConfig {
            source_buffer_address: source_buffer::id(),
            upgrade_authority_address: Some(upgrade_authority::id()),
            feature_id: feature::id(),
            migration_target: super::CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "migrate_builtin_to_core_bpf_system_program",
        };
    }

    pub mod vote_program {
        pub mod feature {
            solana_pubkey::declare_id!("5wDLHMasPmtrcpfRZX67RVkBXBbSTQ9S4C8EJomD3yAk");
        }
        pub mod source_buffer {
            solana_pubkey::declare_id!("6T9s4PTcHnpq2AVAqoCbJd4FuHsdD99MjSUEbS7qb1tT");
        }
        pub mod upgrade_authority {
            solana_pubkey::declare_id!("2N4JfyYub6cWUP9R4JrsFHv6FYKT7JnoRX8GQUH9MdT3");
        }
        pub const CONFIG: super::CoreBpfMigrationConfig = super::CoreBpfMigrationConfig {
            source_buffer_address: source_buffer::id(),
            upgrade_authority_address: Some(upgrade_authority::id()),
            feature_id: feature::id(),
            migration_target: super::CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "migrate_builtin_to_core_bpf_vote_program",
        };
    }

    pub mod solana_bpf_loader_deprecated_program {
        pub mod feature {
            solana_pubkey::declare_id!("8gpakCv5Pk5PZGv9RUjzdkk2GVQPGx12cNRUDMQ3bP86");
        }
        pub mod source_buffer {
            solana_pubkey::declare_id!("DveUYB5m9G3ce4zpV3fxg9pCNkvH1wDsyd8XberZ47JL");
        }
        pub mod upgrade_authority {
            solana_pubkey::declare_id!("8Y5VTHdadnz4rZZWdUA4Qq2m2zWoCwwtb38spPZCXuGU");
        }
        pub const CONFIG: super::CoreBpfMigrationConfig = super::CoreBpfMigrationConfig {
            source_buffer_address: source_buffer::id(),
            upgrade_authority_address: Some(upgrade_authority::id()),
            feature_id: feature::id(),
            migration_target: super::CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "migrate_builtin_to_core_bpf_bpf_loader_deprecated_program",
        };
    }

    pub mod solana_bpf_loader_program {
        pub mod feature {
            solana_pubkey::declare_id!("8yEdUm4SaP1yNq2MczEVdrM48SucvZCTDSqjcAKfYfL6");
        }
        pub mod source_buffer {
            solana_pubkey::declare_id!("2EWMYGJPuGLW4TexLLEMeXP2BkB1PXEKBFb698yw6LhT");
        }
        pub mod upgrade_authority {
            solana_pubkey::declare_id!("3sQ9VZ1Lvuvs6NpFXFV3ByFAf52ajPPdXwuhYERJR3iJ");
        }
        pub const CONFIG: super::CoreBpfMigrationConfig = super::CoreBpfMigrationConfig {
            source_buffer_address: source_buffer::id(),
            upgrade_authority_address: Some(upgrade_authority::id()),
            feature_id: feature::id(),
            migration_target: super::CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "migrate_builtin_to_core_bpf_bpf_loader_program",
        };
    }

    pub mod solana_bpf_loader_upgradeable_program {
        pub mod feature {
            solana_pubkey::declare_id!("oPQbVjgoQ7SaQmzZiiHW4xqHbh4BJqqrFhxEJZiMiwY");
        }
        pub mod source_buffer {
            solana_pubkey::declare_id!("6bTmA9iefD57GDoQ9wUjG8SeYkSpRw3EkKzxZCbhkavq");
        }
        pub mod upgrade_authority {
            solana_pubkey::declare_id!("CuJvJY1K2wx82oLrQGSSWtw4AF7nVifEHupzSC2KEcq5");
        }
        pub const CONFIG: super::CoreBpfMigrationConfig = super::CoreBpfMigrationConfig {
            source_buffer_address: source_buffer::id(),
            upgrade_authority_address: Some(upgrade_authority::id()),
            feature_id: feature::id(),
            migration_target: super::CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "migrate_builtin_to_core_bpf_bpf_loader_upgradeable_program",
        };
    }

    pub mod compute_budget_program {
        pub mod feature {
            solana_pubkey::declare_id!("D39vUspVfhjPVD7EtMJZrA5j1TSMp4LXfb43nxumGdHT");
        }
        pub mod source_buffer {
            solana_pubkey::declare_id!("KfX1oLpFC5CwmFeSgXrNcXaouKjFkPuSJ4UsKb3zKMX");
        }
        pub mod upgrade_authority {
            solana_pubkey::declare_id!("HGTbQhaCXNTbpgpLb2KNjqWSwpJyb2dqDB66Lc3Ph4aN");
        }
        pub const CONFIG: super::CoreBpfMigrationConfig = super::CoreBpfMigrationConfig {
            source_buffer_address: source_buffer::id(),
            upgrade_authority_address: Some(upgrade_authority::id()),
            feature_id: feature::id(),
            migration_target: super::CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "migrate_builtin_to_core_bpf_compute_budget_program",
        };
    }

    pub mod zk_token_proof_program {
        pub mod feature {
            solana_pubkey::declare_id!("GfeFwUzKP9NmaP5u4VfnFgEvQoeQc2wPgnBFrUZhpib5");
        }
        pub mod source_buffer {
            solana_pubkey::declare_id!("Ffe9gL8vXraBkiv3HqbLvBqY7i9V4qtZxjH83jYYDe1V");
        }
        pub mod upgrade_authority {
            solana_pubkey::declare_id!("6zkXWHR8YeCvfMqHwyiz2n7g6hMUKCFhrVccZZTDk4ei");
        }
        pub const CONFIG: super::CoreBpfMigrationConfig = super::CoreBpfMigrationConfig {
            source_buffer_address: source_buffer::id(),
            upgrade_authority_address: Some(upgrade_authority::id()),
            feature_id: feature::id(),
            migration_target: super::CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "migrate_builtin_to_core_bpf_zk_token_proof_program",
        };
    }

    pub mod loader_v4 {
        pub mod feature {
            solana_pubkey::declare_id!("Cz5JthYp27KR3rwTCtVJhbRgwHCurbwcYX46D8setL22");
        }
        pub mod source_buffer {
            solana_pubkey::declare_id!("EH45pKy1kzjifB93wEJi91js3S4HETdsteywR7ZCNPn5");
        }
        pub mod upgrade_authority {
            solana_pubkey::declare_id!("AWbiYRbFts9GVX5uwUkwV46hTFP85PxCAM8e8ir8Hqtq");
        }
        pub const CONFIG: super::CoreBpfMigrationConfig = super::CoreBpfMigrationConfig {
            source_buffer_address: source_buffer::id(),
            upgrade_authority_address: Some(upgrade_authority::id()),
            feature_id: feature::id(),
            migration_target: super::CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "migrate_builtin_to_core_bpf_loader_v4_program",
        };
    }

    pub mod zk_elgamal_proof_program {
        pub mod feature {
            solana_pubkey::declare_id!("EYtuxScWqGWmcPEDmeUsEt3iPkvWE26EWLfSxUvWP2WN");
        }
        pub mod source_buffer {
            solana_pubkey::declare_id!("AaVrLPurAUmjw6XRNGr6ezQfHaJWpBGHhcRSJmNjoVpQ");
        }
        pub mod upgrade_authority {
            solana_pubkey::declare_id!("EyGkQYHgynUdvdNPNiWbJQk9roFCexgdJQMNcWbuvp78");
        }
        pub const CONFIG: super::CoreBpfMigrationConfig = super::CoreBpfMigrationConfig {
            source_buffer_address: source_buffer::id(),
            upgrade_authority_address: Some(upgrade_authority::id()),
            feature_id: feature::id(),
            migration_target: super::CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "migrate_builtin_to_core_bpf_zk_elgamal_proof_program",
        };
    }
}

#[cfg(test)]
mod tests {
    // Since a macro is used to initialize the test IDs from the `test_only`
    // module, best to ensure the lists have the expected values within a test
    // context.
    #[test]
    fn test_testable_prototypes() {
        assert_eq!(
            &super::BUILTINS[0].core_bpf_migration_config,
            &Some(super::test_only::system_program::CONFIG)
        );
        assert_eq!(
            &super::BUILTINS[1].core_bpf_migration_config,
            &Some(super::test_only::vote_program::CONFIG)
        );
        assert_eq!(
            &super::BUILTINS[2].core_bpf_migration_config,
            &Some(super::test_only::solana_bpf_loader_deprecated_program::CONFIG)
        );
        assert_eq!(
            &super::BUILTINS[3].core_bpf_migration_config,
            &Some(super::test_only::solana_bpf_loader_program::CONFIG)
        );
        assert_eq!(
            &super::BUILTINS[4].core_bpf_migration_config,
            &Some(super::test_only::solana_bpf_loader_upgradeable_program::CONFIG)
        );
        assert_eq!(
            &super::BUILTINS[5].core_bpf_migration_config,
            &Some(super::test_only::compute_budget_program::CONFIG)
        );
        assert_eq!(
            &super::BUILTINS[6].core_bpf_migration_config,
            &Some(super::test_only::zk_token_proof_program::CONFIG)
        );
        assert_eq!(
            &super::BUILTINS[7].core_bpf_migration_config,
            &Some(super::test_only::loader_v4::CONFIG)
        );
        assert_eq!(
            &super::BUILTINS[8].core_bpf_migration_config,
            &Some(super::test_only::zk_elgamal_proof_program::CONFIG)
        );
    }
}
