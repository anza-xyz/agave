use {
    crate::{
        cli::thread_args::{RocksdbCompactionThreadsArg, RocksdbFlushThreadsArg, ThreadArg},
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, ArgMatches},
    solana_ledger::blockstore_options::{
        AccessType, BlockstoreCompressionType, BlockstoreOptions, BlockstoreRecoveryMode,
        LedgerColumnOptions,
    },
    std::num::NonZeroUsize,
};

impl FromClapArgMatches for BlockstoreOptions {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let recovery_mode = matches
            .value_of("wal_recovery_mode")
            .map(BlockstoreRecoveryMode::from);

        let column_options = LedgerColumnOptions {
            compression_type: match matches.value_of("rocksdb_ledger_compression") {
                None => BlockstoreCompressionType::default(),
                Some(ledger_compression_string) => match ledger_compression_string {
                    "none" => BlockstoreCompressionType::None,
                    "snappy" => BlockstoreCompressionType::Snappy,
                    "lz4" => BlockstoreCompressionType::Lz4,
                    "zlib" => BlockstoreCompressionType::Zlib,
                    _ => {
                        return Err(crate::commands::Error::Dynamic(
                            Box::<dyn std::error::Error>::from(format!(
                                "Unsupported ledger_compression: {ledger_compression_string}"
                            )),
                        ));
                    }
                },
            },
            rocks_perf_sample_interval: value_t!(matches, "rocksdb_perf_sample_interval", usize)?,
        };

        let rocksdb_compaction_threads =
            value_t!(matches, RocksdbCompactionThreadsArg::NAME, NonZeroUsize)?;

        let rocksdb_flush_threads = value_t!(matches, RocksdbFlushThreadsArg::NAME, NonZeroUsize)?;

        Ok(BlockstoreOptions {
            recovery_mode,
            column_options,
            // The validator needs to open many files, check that the process has
            // permission to do so in order to fail quickly and give a direct error
            enforce_ulimit_nofile: true,
            // The validator needs primary (read/write)
            access_type: AccessType::Primary,
            num_rocksdb_compaction_threads: rocksdb_compaction_threads,
            num_rocksdb_flush_threads: rocksdb_flush_threads,
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::run::args::{
            tests::{
                verify_args_struct_by_command_run_is_error_with_identity_setup,
                verify_args_struct_by_command_run_with_identity_setup,
            },
            RunArgs,
        },
        test_case::test_case,
    };

    #[test_case(
        "tolerate_corrupted_tail_records",
        BlockstoreRecoveryMode::TolerateCorruptedTailRecords
    )]
    #[test_case("absolute_consistency", BlockstoreRecoveryMode::AbsoluteConsistency)]
    #[test_case("point_in_time", BlockstoreRecoveryMode::PointInTime)]
    #[test_case(
        "skip_any_corrupted_record",
        BlockstoreRecoveryMode::SkipAnyCorruptedRecord
    )]
    fn verify_args_struct_by_command_run_with_wal_recovery_mode_valid(
        arg_value: &str,
        expected_mode: BlockstoreRecoveryMode,
    ) {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            blockstore_options: BlockstoreOptions {
                recovery_mode: Some(expected_mode),
                ..default_run_args.blockstore_options.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--wal-recovery-mode", arg_value],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_wal_recovery_mode_invalid() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        verify_args_struct_by_command_run_is_error_with_identity_setup(
            default_run_args,
            vec!["--wal-recovery-mode", "invalid"],
        );
    }

    #[test_case("none", BlockstoreCompressionType::None)]
    #[test_case("snappy", BlockstoreCompressionType::Snappy)]
    #[test_case("lz4", BlockstoreCompressionType::Lz4)]
    #[test_case("zlib", BlockstoreCompressionType::Zlib)]
    fn verify_args_struct_by_command_run_with_rocksdb_ledger_compression(
        arg_value: &str,
        expected_compression: BlockstoreCompressionType,
    ) {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            blockstore_options: BlockstoreOptions {
                column_options: LedgerColumnOptions {
                    compression_type: expected_compression,
                    ..default_run_args.blockstore_options.column_options.clone()
                },
                ..default_run_args.blockstore_options.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rocksdb-ledger-compression", arg_value],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rocksdb_ledger_compression_invalid() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        verify_args_struct_by_command_run_is_error_with_identity_setup(
            default_run_args,
            vec!["--rocksdb-ledger-compression", "invalid"],
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rocksdb_perf_sample_interval() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            blockstore_options: BlockstoreOptions {
                column_options: LedgerColumnOptions {
                    rocks_perf_sample_interval: 100,
                    ..default_run_args.blockstore_options.column_options.clone()
                },
                ..default_run_args.blockstore_options.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rocksdb-perf-sample-interval", "100"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rocksdb_compaction_threads() {
        // long arg
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    num_rocksdb_compaction_threads: NonZeroUsize::new(1).unwrap(),
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rocksdb-compaction-threads", "1"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rocksdb_flush_threads() {
        // long arg
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    num_rocksdb_flush_threads: NonZeroUsize::new(1).unwrap(),
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rocksdb-flush-threads", "1"],
                expected_args,
            );
        }
    }
}
