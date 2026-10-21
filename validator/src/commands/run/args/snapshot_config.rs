use {
    crate::cli::DefaultArgs,
    agave_snapshots::{
        ArchiveFormat, SUPPORTED_ARCHIVE_COMPRESSION, SnapshotInterval, SnapshotVersion,
        paths::BANK_SNAPSHOTS_DIR,
        snapshot_config::{SnapshotConfig, SnapshotUsage},
    },
    clap::{Arg, ArgMatches, value_t},
    log::{info, warn},
    solana_clap_utils::input_validators::{
        is_non_zero, is_parsable, validate_maximum_full_snapshot_archives_to_retain,
        validate_maximum_incremental_snapshot_archives_to_retain,
    },
    solana_clock::DEFAULT_SLOTS_PER_EPOCH,
    solana_core::{resource_limits, validator::is_snapshot_config_valid},
    std::{
        error::Error,
        num::{NonZeroU64, NonZeroUsize},
        path::{Path, PathBuf},
    },
};

pub(crate) struct SnapshotOptions {
    pub(crate) incremental_snapshot_fetch: bool,
    pub(crate) use_direct_io: bool,
}

/// The snapshot directories are created and canonicalized by `storage::prepare`
pub(crate) fn new_config(
    matches: &ArgMatches,
    ledger_path: &Path,
    options: SnapshotOptions,
) -> Result<SnapshotConfig, Box<dyn Error>> {
    let SnapshotOptions {
        incremental_snapshot_fetch,
        use_direct_io,
    } = options;
    let (full_snapshot_archive_interval, incremental_snapshot_archive_interval) =
        if matches.is_present("no_snapshots") {
            // snapshots are disabled
            (SnapshotInterval::Disabled, SnapshotInterval::Disabled)
        } else {
            match (
                incremental_snapshot_fetch,
                value_t!(matches, "snapshot_interval_slots", NonZeroU64)?,
            ) {
                (true, incremental_snapshot_interval_slots) => {
                    // incremental snapshots are enabled
                    // use --snapshot-interval-slots for the incremental snapshot interval
                    let full_snapshot_interval_slots =
                        value_t!(matches, "full_snapshot_interval_slots", NonZeroU64)?;
                    (
                        SnapshotInterval::Slots(full_snapshot_interval_slots),
                        SnapshotInterval::Slots(incremental_snapshot_interval_slots),
                    )
                }
                (false, full_snapshot_interval_slots) => {
                    // incremental snapshots are *disabled*
                    // use --snapshot-interval-slots for the *full* snapshot interval
                    // also warn if --full-snapshot-interval-slots was specified
                    if matches.occurrences_of("full_snapshot_interval_slots") > 0 {
                        warn!(
                            "Incremental snapshots are disabled, yet \
                             --full-snapshot-interval-slots was specified! Note that \
                             --full-snapshot-interval-slots is *ignored* when incremental \
                             snapshots are disabled. Use --snapshot-interval-slots instead.",
                        );
                    }
                    (
                        SnapshotInterval::Slots(full_snapshot_interval_slots),
                        SnapshotInterval::Disabled,
                    )
                }
            }
        };

    info!(
        "Snapshot configuration: full snapshot interval: {}, incremental snapshot interval: {}",
        match full_snapshot_archive_interval {
            SnapshotInterval::Disabled => "disabled".to_string(),
            SnapshotInterval::Slots(interval) => format!("{interval} slots"),
        },
        match incremental_snapshot_archive_interval {
            SnapshotInterval::Disabled => "disabled".to_string(),
            SnapshotInterval::Slots(interval) => format!("{interval} slots"),
        },
    );
    // It is unlikely that a full snapshot interval greater than an epoch is a good idea.
    // Minimally we should warn the user in case this was a mistake.
    if let SnapshotInterval::Slots(full_snapshot_interval_slots) = full_snapshot_archive_interval {
        let full_snapshot_interval_slots = full_snapshot_interval_slots.get();
        if full_snapshot_interval_slots > DEFAULT_SLOTS_PER_EPOCH {
            warn!(
                "The full snapshot interval is excessively large: {full_snapshot_interval_slots}! \
                 This will negatively impact the background cleanup tasks in accounts-db. \
                 Consider a smaller value.",
            );
        }
    }

    let snapshots_dir = matches
        .value_of("snapshots")
        .map(Path::new)
        .unwrap_or(ledger_path);
    let bank_snapshots_dir = snapshots_dir.join(BANK_SNAPSHOTS_DIR);
    let full_snapshot_archives_dir = matches
        .value_of("full_snapshot_archive_path")
        .map(PathBuf::from)
        .unwrap_or_else(|| snapshots_dir.to_path_buf());
    let incremental_snapshot_archives_dir = matches
        .value_of("incremental_snapshot_archive_path")
        .map(PathBuf::from)
        .unwrap_or_else(|| snapshots_dir.to_path_buf());

    let archive_format = {
        let archive_format_str = value_t!(matches, "snapshot_archive_format", String)?;
        let mut archive_format = ArchiveFormat::from_cli_arg(&archive_format_str)
            .ok_or_else(|| format!("Archive format not recognized: {archive_format_str}"))?;
        if let ArchiveFormat::TarZstd { config } = &mut archive_format {
            config.compression_level = value_t!(matches, "snapshot_zstd_compression_level", i32)?;
        }
        archive_format
    };

    let snapshot_version = matches
        .value_of("snapshot_version")
        .map(|value| {
            value
                .parse::<SnapshotVersion>()
                .map_err(|err| format!("unable to parse snapshot version: {err}"))
        })
        .transpose()?
        .unwrap_or(SnapshotVersion::default());

    let maximum_full_snapshot_archives_to_retain =
        value_t!(matches, "maximum_full_snapshots_to_retain", NonZeroUsize)?;
    let maximum_incremental_snapshot_archives_to_retain = value_t!(
        matches,
        "maximum_incremental_snapshots_to_retain",
        NonZeroUsize
    )?;

    let snapshot_config = SnapshotConfig {
        usage: if full_snapshot_archive_interval == SnapshotInterval::Disabled {
            SnapshotUsage::LoadOnly
        } else {
            SnapshotUsage::LoadAndGenerate
        },
        full_snapshot_archive_interval,
        incremental_snapshot_archive_interval,
        bank_snapshots_dir,
        full_snapshot_archives_dir,
        incremental_snapshot_archives_dir,
        archive_format,
        snapshot_version,
        maximum_full_snapshot_archives_to_retain,
        maximum_incremental_snapshot_archives_to_retain,
        use_registered_io_uring_buffers: resource_limits::check_memlock_limit_for_disk_io(
            solana_accounts_db::accounts_db::TOTAL_IO_URING_BUFFERS_SIZE_LIMIT,
        ),
        use_direct_io,
    };

    if !is_snapshot_config_valid(&snapshot_config) {
        Err(
            "invalid snapshot configuration provided: snapshot intervals are incompatible. full \
             snapshot interval MUST be larger than incremental snapshot interval (if enabled)"
                .to_string(),
        )?;
    }

    Ok(snapshot_config)
}

pub(crate) fn args(default_args: &DefaultArgs) -> Vec<Arg<'_, '_>> {
    vec![
        Arg::with_name("snapshots")
            .long("snapshots")
            .value_name("DIR")
            .takes_value(true)
            .help("Use DIR as the base location for snapshots.")
            .long_help(
                "Use DIR as the base location for snapshots. Snapshot archives will use DIR \
                 unless --full-snapshot-archive-path or --incremental-snapshot-archive-path is \
                 specified. Additionally, a subdirectory named \"snapshots\" will be created in \
                 DIR. This subdirectory holds internal files/data that are used when generating \
                 snapshot archives. [default: --ledger value]",
            ),
        Arg::with_name("full_snapshot_archive_path")
            .long("full-snapshot-archive-path")
            .value_name("DIR")
            .takes_value(true)
            .help("Use DIR as full snapshot archives location [default: --snapshots value]"),
        Arg::with_name("incremental_snapshot_archive_path")
            .long("incremental-snapshot-archive-path")
            .conflicts_with("no_incremental_snapshots")
            .value_name("DIR")
            .takes_value(true)
            .help("Use DIR as incremental snapshot archives location [default: --snapshots value]"),
        Arg::with_name("no_snapshots")
            .long("no-snapshots")
            .takes_value(false)
            .conflicts_with_all(&[
                "no_incremental_snapshots",
                "snapshot_interval_slots",
                "full_snapshot_interval_slots",
            ])
            .help("Disable all snapshot generation"),
        Arg::with_name("snapshot_interval_slots")
            .long("snapshot-interval-slots")
            .alias("incremental-snapshot-interval-slots")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.incremental_snapshot_archive_interval_slots)
            .validator(is_non_zero)
            .help("Number of slots between generating snapshots")
            .long_help(
                "Number of slots between generating snapshots. If incremental snapshots are \
                 enabled, this sets the incremental snapshot interval. If incremental snapshots \
                 are disabled, this sets the full snapshot interval. Must be greater than zero.",
            ),
        Arg::with_name("full_snapshot_interval_slots")
            .long("full-snapshot-interval-slots")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.full_snapshot_archive_interval_slots)
            .validator(is_non_zero)
            .help("Number of slots between generating full snapshots")
            .long_help(
                "Number of slots between generating full snapshots. Only used when incremental \
                 snapshots are enabled. Must be greater than the incremental snapshot interval. \
                 Must be greater than zero.",
            ),
        Arg::with_name("maximum_full_snapshots_to_retain")
            .long("maximum-full-snapshots-to-retain")
            .alias("maximum-snapshots-to-retain")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.maximum_full_snapshot_archives_to_retain)
            .validator(validate_maximum_full_snapshot_archives_to_retain)
            .help(
                "The maximum number of full snapshot archives to hold on to when purging older \
                 snapshots.",
            ),
        Arg::with_name("maximum_incremental_snapshots_to_retain")
            .long("maximum-incremental-snapshots-to-retain")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.maximum_incremental_snapshot_archives_to_retain)
            .validator(validate_maximum_incremental_snapshot_archives_to_retain)
            .help(
                "The maximum number of incremental snapshot archives to hold on to when purging \
                 older snapshots.",
            ),
        Arg::with_name("snapshot_version")
            .long("snapshot-version")
            .value_name("SNAPSHOT_VERSION")
            .validator(is_parsable::<SnapshotVersion>)
            .takes_value(true)
            .default_value(default_args.snapshot_version.into())
            .help("Output snapshot version"),
        Arg::with_name("snapshot_archive_format")
            .long("snapshot-archive-format")
            .alias("snapshot-compression") // Legacy name used by Solana v1.5.x and older
            .possible_values(SUPPORTED_ARCHIVE_COMPRESSION)
            .default_value(&default_args.snapshot_archive_format)
            .value_name("ARCHIVE_TYPE")
            .takes_value(true)
            .help("Snapshot archive format to use."),
        Arg::with_name("snapshot_zstd_compression_level")
            .long("snapshot-zstd-compression-level")
            .default_value(&default_args.snapshot_zstd_compression_level)
            .value_name("LEVEL")
            .takes_value(true)
            .help("The compression level to use when archiving with zstd")
            .long_help(
                "The compression level to use when archiving with zstd. Higher compression levels \
                 generally produce higher compression ratio at the expense of speed and memory. \
                 See the zstd manpage for more information.",
            ),
    ]
}

#[cfg(test)]
mod tests {
    use {super::*, crate::cli::app, test_case::test_case};

    fn new_test_config(
        incremental_snapshot_fetch: bool,
        args: &[&str],
    ) -> Result<SnapshotConfig, Box<dyn Error>> {
        let default_args = DefaultArgs::default();
        let matches =
            app("test", &default_args).get_matches_from(["agave-validator"].iter().chain(args));
        new_config(
            &matches,
            Path::new("ledger"),
            SnapshotOptions {
                incremental_snapshot_fetch,
                use_direct_io: true,
            },
        )
    }

    fn slots(slots: u64) -> SnapshotInterval {
        SnapshotInterval::Slots(NonZeroU64::new(slots).unwrap())
    }

    #[test_case(true, &["--full-snapshot-interval-slots", "400", "--snapshot-interval-slots", "100"], slots(400), slots(100); "incremental")]
    #[test_case(false, &["--snapshot-interval-slots", "100"], slots(100), SnapshotInterval::Disabled; "full_only")]
    #[test_case(true, &["--no-snapshots"], SnapshotInterval::Disabled, SnapshotInterval::Disabled; "disabled")]
    fn test_snapshot_intervals(
        incremental_snapshot_fetch: bool,
        args: &[&str],
        full: SnapshotInterval,
        incremental: SnapshotInterval,
    ) {
        let config = new_test_config(incremental_snapshot_fetch, args).unwrap();
        assert_eq!(
            (
                config.full_snapshot_archive_interval,
                config.incremental_snapshot_archive_interval
            ),
            (full, incremental)
        );
    }

    #[test]
    fn test_full_interval_must_exceed_incremental_interval() {
        let args = [
            "--full-snapshot-interval-slots",
            "100",
            "--snapshot-interval-slots",
            "400",
        ];
        assert!(new_test_config(true, &args).is_err());
    }

    #[test]
    fn test_snapshot_paths() {
        let config = new_test_config(
            true,
            &[
                "--snapshots",
                "snapshots",
                "--incremental-snapshot-archive-path",
                "incremental",
            ],
        )
        .unwrap();
        assert_eq!(
            config.bank_snapshots_dir,
            Path::new("snapshots").join(BANK_SNAPSHOTS_DIR)
        );
        assert_eq!(config.full_snapshot_archives_dir, Path::new("snapshots"));
        assert_eq!(
            config.incremental_snapshot_archives_dir,
            Path::new("incremental")
        );
        assert!(config.use_direct_io);
    }
}
