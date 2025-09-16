use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, values_t, ArgMatches},
    solana_accounts_db::{
        accounts_db::{AccountShrinkThreshold, AccountsDbConfig},
        accounts_index::{AccountSecondaryIndexes, AccountsIndexConfig},
        utils::{create_all_accounts_run_and_snapshot_dirs, create_and_canonicalize_directories},
    },
    std::path::PathBuf,
};

impl FromClapArgMatches for AccountsDbConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
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

        Ok(AccountsDbConfig {
            index: Some(accounts_index_config),
            account_indexes: Some(account_indexes),
            shrink_paths: account_shrink_run_paths,
            shrink_ratio,
            ..Default::default()
        })
    }
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
}
