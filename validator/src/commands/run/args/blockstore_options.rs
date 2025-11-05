use {
    crate::{
        cli::thread_args::{new_thread_arg, ThreadArg},
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, Arg, ArgMatches},
    solana_clap_utils::{hidden_unless_forced, input_validators::is_parsable},
    solana_ledger::blockstore_options::{
        AccessType, BlockstoreCompressionType, BlockstoreOptions, BlockstoreRecoveryMode,
        LedgerColumnOptions,
    },
    std::{num::NonZeroUsize, sync::LazyLock},
};

struct RocksdbCompactionThreadsArg;
impl ThreadArg for RocksdbCompactionThreadsArg {
    const NAME: &'static str = "rocksdb_compaction_threads";
    const LONG_NAME: &'static str = "rocksdb-compaction-threads";
    const HELP: &'static str = "Number of threads to use for rocksdb (Blockstore) compactions";

    fn default() -> usize {
        solana_ledger::blockstore::default_num_compaction_threads().get()
    }
}

struct RocksdbFlushThreadsArg;
impl ThreadArg for RocksdbFlushThreadsArg {
    const NAME: &'static str = "rocksdb_flush_threads";
    const LONG_NAME: &'static str = "rocksdb-flush-threads";
    const HELP: &'static str = "Number of threads to use for rocksdb (Blockstore) memtable flushes";

    fn default() -> usize {
        solana_ledger::blockstore::default_num_flush_threads().get()
    }
}

const DEFAULT_ROCKSDB_LEDGER_COMPRESSION: &str = "none";
const DEFAULT_ROCKSDB_PERF_SAMPLE_INTERVAL: &str = "0";
static DEFAULT_ROCKSDB_COMPACTION_THREADS: LazyLock<String> =
    LazyLock::new(|| RocksdbCompactionThreadsArg::default().to_string());
static DEFAULT_ROCKSDB_FLUSH_THREADS: LazyLock<String> =
    LazyLock::new(|| RocksdbFlushThreadsArg::default().to_string());
const DEFAULT_ROCKSDB_SHRED_COMPACTION: &str = "level";

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
            // The validator needs primary (read/write)
            access_type: AccessType::Primary,
            num_rocksdb_compaction_threads: rocksdb_compaction_threads,
            num_rocksdb_flush_threads: rocksdb_flush_threads,
        })
    }
}

pub(crate) fn args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::with_name("wal_recovery_mode")
            .long("wal-recovery-mode")
            .value_name("MODE")
            .takes_value(true)
            .possible_values(&[
                "tolerate_corrupted_tail_records",
                "absolute_consistency",
                "point_in_time",
                "skip_any_corrupted_record",
            ])
            .help("Mode to recovery the ledger db write ahead log."),
        Arg::with_name("rocksdb_ledger_compression")
            .hidden(hidden_unless_forced())
            .long("rocksdb-ledger-compression")
            .value_name("COMPRESSION_TYPE")
            .takes_value(true)
            .possible_values(&["none", "lz4", "snappy", "zlib"])
            .default_value(DEFAULT_ROCKSDB_LEDGER_COMPRESSION)
            .help(
                "The compression algorithm that is used to compress transaction status data. \
                 Turning on compression can save ~10% of the ledger size.",
            ),
        Arg::with_name("rocksdb_perf_sample_interval")
            .hidden(hidden_unless_forced())
            .long("rocksdb-perf-sample-interval")
            .value_name("ROCKS_PERF_SAMPLE_INTERVAL")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .default_value(DEFAULT_ROCKSDB_PERF_SAMPLE_INTERVAL)
            .help(
                "Controls how often RocksDB read/write performance samples are collected. Perf \
                 samples are collected in 1 / ROCKS_PERF_SAMPLE_INTERVAL sampling rate.",
            ),
        new_thread_arg::<RocksdbCompactionThreadsArg>(&DEFAULT_ROCKSDB_COMPACTION_THREADS),
        new_thread_arg::<RocksdbFlushThreadsArg>(&DEFAULT_ROCKSDB_FLUSH_THREADS),
        Arg::with_name("rocksdb_shred_compaction")
            .long("rocksdb-shred-compaction")
            .value_name("ROCKSDB_COMPACTION_STYLE")
            .takes_value(true)
            .possible_values(&["level"])
            .default_value(DEFAULT_ROCKSDB_SHRED_COMPACTION)
            .help(
                "Controls how RocksDB compacts shreds. *WARNING*: You will lose your Blockstore \
                 data when you switch between options.",
            ),
        Arg::with_name("limit_ledger_size")
            .long("limit-ledger-size")
            .value_name("SHRED_COUNT")
            .takes_value(true)
            .min_values(0)
            .max_values(1)
            /* .default_value() intentionally not used here! */
            .help("Keep this amount of shreds in root slots."),
    ]
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
        std::ops::RangeInclusive,
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

    #[test]
    fn test_default_rocksdb_ledger_compression_unchanged() {
        assert_eq!(DEFAULT_ROCKSDB_LEDGER_COMPRESSION, "none");
    }

    #[test]
    fn test_default_rocksdb_perf_sample_interval_unchanged() {
        assert_eq!(DEFAULT_ROCKSDB_PERF_SAMPLE_INTERVAL, "0");
    }

    #[test]
    fn test_default_rocksdb_compaction_threads_unchanged() {
        assert_eq!(
            *DEFAULT_ROCKSDB_COMPACTION_THREADS,
            num_cpus::get().to_string(),
        );
    }

    #[test]
    fn test_valid_range_rocksdb_compaction_threads_unchanged() {
        assert_eq!(
            RocksdbCompactionThreadsArg::range(),
            RangeInclusive::new(1, num_cpus::get()),
        );
    }

    #[test]
    fn test_default_rocksdb_flush_threads_unchanged() {
        assert_eq!(
            *DEFAULT_ROCKSDB_FLUSH_THREADS,
            (num_cpus::get() / 4).max(1).to_string()
        );
    }

    #[test]
    fn test_valid_range_rocksdb_flush_threads_unchanged() {
        assert_eq!(
            RocksdbFlushThreadsArg::range(),
            RangeInclusive::new(1, num_cpus::get()),
        );
    }

    #[test]
    fn test_default_rocksdb_shred_compaction_unchanged() {
        assert_eq!(DEFAULT_ROCKSDB_SHRED_COMPACTION, "level");
    }
}
