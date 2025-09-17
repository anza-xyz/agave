use {
    crate::{
        cli::thread_args::{
            AccountsDbBackgroundThreadsArg, AccountsDbForegroundThreadsArg, ThreadArg,
        },
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, values_t, ArgMatches},
    solana_accounts_db::{
        accounts_db::{AccountShrinkThreshold, AccountsDbConfig, MarkObsoleteAccounts},
        accounts_file::StorageAccess,
        accounts_index::{AccountSecondaryIndexes, AccountsIndexConfig, ScanFilter},
        utils::{create_all_accounts_run_and_snapshot_dirs, create_and_canonicalize_directories},
    },
    solana_clap_utils::input_parsers::values_of,
    std::{num::NonZeroUsize, path::PathBuf},
};

pub fn new_accounts_db_config(
    matches: &ArgMatches,
    ledger_path: &PathBuf,
) -> Result<AccountsDbConfig> {
    let accounts_index_config = AccountsIndexConfig::from_clap_arg_match(matches)?;

    let account_indexes = AccountSecondaryIndexes::from_clap_arg_match(matches)?;

    let (account_shrink_run_paths, _) = parse_account_shrink_paths(matches)?;

    let accounts_shrink_optimize_total_space =
        value_t!(matches, "accounts_shrink_optimize_total_space", bool)?;
    let shrink_ratio = value_t!(matches, "accounts_shrink_ratio", f64)?;
    if !(0.0..=1.0).contains(&shrink_ratio) {
        return Err(crate::commands::Error::Dynamic(
            Box::<dyn std::error::Error>::from(format!(
                "the specified account-shrink-ratio is invalid, it must be between 0. and 1.0 \
             inclusive: {shrink_ratio}"
            )),
        ));
    }

    let shrink_ratio = if accounts_shrink_optimize_total_space {
        AccountShrinkThreshold::TotalSpace { shrink_ratio }
    } else {
        AccountShrinkThreshold::IndividualStore { shrink_ratio }
    };

    const MIB: usize = 1_024 * 1_024;

    let read_cache_limit_bytes =
        values_of::<usize>(matches, "accounts_db_read_cache_limit").map(|limits| {
            match limits.len() {
                2 => (limits[0], limits[1]),
                _ => {
                    // clap will enforce two values are given
                    unreachable!("invalid number of values given to accounts-db-read-cache-limit")
                }
            }
        });
    // accounts-db-read-cache-limit-mb was deprecated in v3.0.0
    let read_cache_limit_mb =
        values_of::<usize>(matches, "accounts_db_read_cache_limit_mb").map(|limits| {
            match limits.len() {
                // we were given explicit low and high watermark values, so use them
                2 => (limits[0] * MIB, limits[1] * MIB),
                // we were given a single value, so use it for both low and high watermarks
                1 => (limits[0] * MIB, limits[0] * MIB),
                _ => {
                    // clap will enforce either one or two values is given
                    unreachable!(
                        "invalid number of values given to accounts-db-read-cache-limit-mb"
                    )
                }
            }
        });
    // clap will enforce only one cli arg is provided, so pick whichever is Some
    let read_cache_limit_bytes = read_cache_limit_bytes.or(read_cache_limit_mb);

    let write_cache_limit_bytes = value_t!(matches, "accounts_db_cache_limit_mb", u64)
        .ok()
        .map(|mb| mb * MIB as u64);

    let ancient_append_vec_offset = value_t!(matches, "accounts_db_ancient_append_vecs", i64).ok();

    let ancient_storage_ideal_size =
        value_t!(matches, "accounts_db_ancient_storage_ideal_size", u64).ok();

    let max_ancient_storages = value_t!(matches, "accounts_db_max_ancient_storages", usize).ok();

    let exhaustively_verify_refcounts = matches.is_present("accounts_db_verify_refcounts");

    let storage_access = matches
        .value_of("accounts_db_access_storages_method")
        .map(|method| match method {
            "mmap" => StorageAccess::Mmap,
            "file" => StorageAccess::File,
            _ => {
                // clap will enforce one of the above values is given
                unreachable!("invalid value given to accounts-db-access-storages-method")
            }
        })
        .unwrap_or_default();

    let scan_filter_for_shrinking = matches
        .value_of("accounts_db_scan_filter_for_shrinking")
        .map(|filter| match filter {
            "all" => ScanFilter::All,
            "only-abnormal" => ScanFilter::OnlyAbnormal,
            "only-abnormal-with-verify" => ScanFilter::OnlyAbnormalWithVerify,
            _ => {
                // clap will enforce one of the above values is given
                unreachable!("invalid value given to accounts_db_scan_filter_for_shrinking")
            }
        })
        .unwrap_or_default();

    let accounts_db_background_threads = {
        if matches.is_present("accounts_db_clean_threads") {
            value_t!(matches, "accounts_db_clean_threads", NonZeroUsize)?
        } else {
            value_t!(matches, AccountsDbBackgroundThreadsArg::NAME, NonZeroUsize)?
        }
    };

    let accounts_db_foreground_threads =
        value_t!(matches, AccountsDbForegroundThreadsArg::NAME, NonZeroUsize)?;

    let mark_obsolete_accounts = if matches.is_present("accounts_db_mark_obsolete_accounts") {
        MarkObsoleteAccounts::Enabled
    } else {
        MarkObsoleteAccounts::Disabled
    };

    Ok(AccountsDbConfig {
        index: Some(accounts_index_config),
        account_indexes: Some(account_indexes),
        base_working_path: Some(ledger_path.clone()),
        shrink_paths: account_shrink_run_paths,
        shrink_ratio,
        read_cache_limit_bytes,
        write_cache_limit_bytes,
        ancient_append_vec_offset,
        ancient_storage_ideal_size,
        max_ancient_storages,
        exhaustively_verify_refcounts,
        storage_access,
        scan_filter_for_shrinking,
        num_background_threads: Some(accounts_db_background_threads),
        num_foreground_threads: Some(accounts_db_foreground_threads),
        mark_obsolete_accounts,
        memlock_budget_size: solana_accounts_db::accounts_db::DEFAULT_MEMLOCK_BUDGET_SIZE,
        ..Default::default()
    })
}

pub fn parse_account_shrink_paths(
    matches: &ArgMatches,
) -> Result<(Option<Vec<PathBuf>>, Option<Vec<PathBuf>>)> {
    let account_shrink_paths: Option<Vec<PathBuf>> =
        values_t!(matches, "account_shrink_path", String)
            .map(|shrink_paths| shrink_paths.into_iter().map(PathBuf::from).collect())
            .ok();
    let account_shrink_paths = account_shrink_paths
        .as_ref()
        .map(|paths| {
            create_and_canonicalize_directories(paths).map_err(|err| {
                crate::commands::Error::Dynamic(Box::<dyn std::error::Error>::from(format!(
                    "unable to access account shrink path: {err}"
                )))
            })
        })
        .transpose()?;

    let (account_shrink_run_paths, account_shrink_snapshot_paths) = account_shrink_paths
        .map(|paths| {
            create_all_accounts_run_and_snapshot_dirs(&paths).map_err(|err| {
                crate::commands::Error::Dynamic(Box::<dyn std::error::Error>::from(format!(
                    "unable to create account subdirectories: {err}"
                )))
            })
        })
        .transpose()?
        .unzip();

    Ok((account_shrink_run_paths, account_shrink_snapshot_paths))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::run::args::{
            tests::verify_args_struct_by_command_run_with_identity_setup, RunArgs,
        },
        solana_accounts_db::{accounts_db::AccountsDbConfig, utils::ACCOUNTS_RUN_DIR},
        test_case::test_case,
    };

    #[test]
    fn verify_args_struct_by_command_run_with_account_shrink_path() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let tmp_dir = tempfile::tempdir().unwrap();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                shrink_paths: Some(vec![tmp_dir
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join(ACCOUNTS_RUN_DIR)]),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--account-shrink-path", tmp_dir.path().to_str().unwrap()],
            expected_args,
        );
    }

    #[test_case("true", "0.2", AccountShrinkThreshold::TotalSpace { shrink_ratio: 0.2 })]
    #[test_case("true", "0.5", AccountShrinkThreshold::TotalSpace { shrink_ratio: 0.5 })]
    #[test_case("false", "0.5", AccountShrinkThreshold::IndividualStore { shrink_ratio: 0.5 })]
    fn verify_args_struct_by_command_run_with_accounts_shrink_optimize_total_space(
        accounts_shrink_optimize_total_space: &str,
        accounts_shrink_ratio: &str,
        expected_shrink_ratio: AccountShrinkThreshold,
    ) {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                shrink_ratio: expected_shrink_ratio,
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--accounts-shrink-optimize-total-space",
                accounts_shrink_optimize_total_space,
                "--accounts-shrink-ratio",
                accounts_shrink_ratio,
            ],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_db_read_cache_limit() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                read_cache_limit_bytes: Some((1_000_000, 10_000_000)),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-db-read-cache-limit", "1000000,10000000"],
            expected_args,
        );
    }

    #[test_case("1,10", (1 * 1024 * 1024, 10 * 1024 * 1024))]
    #[test_case("1", (1 * 1024 * 1024, 1 * 1024 * 1024))]
    fn verify_args_struct_by_command_run_with_accounts_db_read_cache_limit_mb(
        accounts_db_read_cache_limit_mb: &str,
        expected_read_cache_limit_bytes: (usize, usize),
    ) {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                read_cache_limit_bytes: Some(expected_read_cache_limit_bytes),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--accounts-db-read-cache-limit-mb",
                accounts_db_read_cache_limit_mb,
            ],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_db_cache_limit_mb() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                write_cache_limit_bytes: Some(10 * 1024 * 1024),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-db-cache-limit-mb", "10"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_db_ancient_append_vecs() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                ancient_append_vec_offset: Some(999_999),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-db-ancient-append-vecs", "999999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_db_ancient_storage_ideal_size() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                ancient_storage_ideal_size: Some(999_999),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-db-ancient-storage-ideal-size", "999999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_db_max_ancient_storages() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                max_ancient_storages: Some(999_999),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-db-max-ancient-storages", "999999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_db_verify_refcounts() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                exhaustively_verify_refcounts: true,
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-db-verify-refcounts"],
            expected_args,
        );
    }

    #[test_case("mmap", StorageAccess::Mmap)]
    #[test_case("file", StorageAccess::File)]
    fn verify_args_struct_by_command_run_with_accounts_db_access_storages_method(
        accounts_db_access_storages_method: &str,
        expected_storage_access: StorageAccess,
    ) {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                storage_access: expected_storage_access,
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--accounts-db-access-storages-method",
                accounts_db_access_storages_method,
            ],
            expected_args,
        );
    }

    #[test_case("all", ScanFilter::All)]
    #[test_case("only-abnormal", ScanFilter::OnlyAbnormal)]
    #[test_case("only-abnormal-with-verify", ScanFilter::OnlyAbnormalWithVerify)]
    fn verify_args_struct_by_command_run_with_accounts_db_scan_filter_for_shrinking(
        accounts_db_scan_filter_for_shrinking: &str,
        expected_scan_filter_for_shrinking: ScanFilter,
    ) {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                scan_filter_for_shrinking: expected_scan_filter_for_shrinking,
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--accounts-db-scan-filter-for-shrinking",
                accounts_db_scan_filter_for_shrinking,
            ],
            expected_args,
        );
    }

    #[test_case("--accounts-db-clean-threads", "2", NonZeroUsize::new(2).unwrap())]
    #[test_case("--accounts-db-background-threads", "2", NonZeroUsize::new(2).unwrap())]
    fn verify_args_struct_by_command_run_with_accounts_db_background_threads(
        accounts_db_background_threads: &str,
        accounts_db_background_threads_value: &str,
        expected_num_background_threads: NonZeroUsize,
    ) {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                num_background_threads: Some(expected_num_background_threads),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                accounts_db_background_threads,
                accounts_db_background_threads_value,
            ],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_db_foreground_threads() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                num_foreground_threads: Some(NonZeroUsize::new(2).unwrap()),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-db-foreground-threads", "2"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_db_mark_obsolete_accounts() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                mark_obsolete_accounts: MarkObsoleteAccounts::Enabled,
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-db-mark-obsolete-accounts"],
            expected_args,
        );
    }
}
