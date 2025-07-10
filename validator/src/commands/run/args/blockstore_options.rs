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
            rocks_perf_sample_interval: value_t!(matches, "rocksdb_perf_sample_interval", usize)
                .map_err(|err| {
                    Box::<dyn std::error::Error>::from(format!(
                        "failed to parse rocksdb_perf_sample_interval: {err}"
                    ))
                })?,
        };

        let rocksdb_compaction_threads =
            value_t!(matches, RocksdbCompactionThreadsArg::NAME, NonZeroUsize).map_err(|err| {
                Box::<dyn std::error::Error>::from(format!(
                    "failed to parse rocksdb_compaction_threads: {err}"
                ))
            })?;

        let rocksdb_flush_threads = value_t!(matches, RocksdbFlushThreadsArg::NAME, NonZeroUsize)
            .map_err(|err| {
            Box::<dyn std::error::Error>::from(format!(
                "failed to parse rocksdb_flush_threads: {err}"
            ))
        })?;

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
            tests::verify_args_struct_by_command_run_with_identity_setup, RunArgs,
        },
    };

    #[test]
    fn verify_args_struct_by_command_run_with_wal_recovery_mode() {
        // tolerate_corrupted_tail_records
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    recovery_mode: Some(BlockstoreRecoveryMode::TolerateCorruptedTailRecords),
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--wal-recovery-mode", "tolerate_corrupted_tail_records"],
                expected_args,
            );
        }

        // absolute_consistency
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    recovery_mode: Some(BlockstoreRecoveryMode::AbsoluteConsistency),
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--wal-recovery-mode", "absolute_consistency"],
                expected_args,
            );
        }

        // point_in_time
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    recovery_mode: Some(BlockstoreRecoveryMode::PointInTime),
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--wal-recovery-mode", "point_in_time"],
                expected_args,
            );
        }

        // skip_any_corrupted_record
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    recovery_mode: Some(BlockstoreRecoveryMode::SkipAnyCorruptedRecord),
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--wal-recovery-mode", "skip_any_corrupted_record"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rocksdb_ledger_compression() {
        // none
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    column_options: LedgerColumnOptions {
                        compression_type: BlockstoreCompressionType::None,
                        ..default_run_args.blockstore_options.column_options.clone()
                    },
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rocksdb-ledger-compression", "none"],
                expected_args,
            );
        }

        // snappy
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    column_options: LedgerColumnOptions {
                        compression_type: BlockstoreCompressionType::Snappy,
                        ..default_run_args.blockstore_options.column_options.clone()
                    },
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rocksdb-ledger-compression", "snappy"],
                expected_args,
            );
        }

        // lz4
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    column_options: LedgerColumnOptions {
                        compression_type: BlockstoreCompressionType::Lz4,
                        ..default_run_args.blockstore_options.column_options.clone()
                    },
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rocksdb-ledger-compression", "lz4"],
                expected_args,
            );
        }

        // zlib
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                blockstore_options: BlockstoreOptions {
                    column_options: LedgerColumnOptions {
                        compression_type: BlockstoreCompressionType::Zlib,
                        ..default_run_args.blockstore_options.column_options.clone()
                    },
                    ..default_run_args.blockstore_options.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rocksdb-ledger-compression", "zlib"],
                expected_args,
            );
        }
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
}
