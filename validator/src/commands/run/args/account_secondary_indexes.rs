use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{values_t, ArgMatches},
    solana_accounts_db::accounts_index::{
        AccountIndex, AccountSecondaryIndexes, AccountSecondaryIndexesIncludeExclude,
    },
    solana_pubkey::Pubkey,
    std::collections::HashSet,
};

impl FromClapArgMatches for AccountSecondaryIndexes {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let account_indexes: HashSet<AccountIndex> = matches
            .values_of("account_indexes")
            .unwrap_or_default()
            .map(|value| match value {
                "program-id" => AccountIndex::ProgramId,
                "spl-token-mint" => AccountIndex::SplTokenMint,
                "spl-token-owner" => AccountIndex::SplTokenOwner,
                _ => unreachable!(),
            })
            .collect();

        let account_indexes_include_keys: HashSet<Pubkey> =
            values_t!(matches, "account_index_include_key", Pubkey)
                .unwrap_or_default()
                .iter()
                .cloned()
                .collect();

        let account_indexes_exclude_keys: HashSet<Pubkey> =
            values_t!(matches, "account_index_exclude_key", Pubkey)
                .unwrap_or_default()
                .iter()
                .cloned()
                .collect();

        let exclude_keys = !account_indexes_exclude_keys.is_empty();
        let include_keys = !account_indexes_include_keys.is_empty();

        let keys = if !account_indexes.is_empty() && (exclude_keys || include_keys) {
            let account_indexes_keys = AccountSecondaryIndexesIncludeExclude {
                exclude: exclude_keys,
                keys: if exclude_keys {
                    account_indexes_exclude_keys
                } else {
                    account_indexes_include_keys
                },
            };
            Some(account_indexes_keys)
        } else {
            None
        };

        Ok(AccountSecondaryIndexes {
            keys,
            indexes: account_indexes,
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
        solana_accounts_db::accounts_db::AccountsDbConfig,
        solana_rpc::rpc::JsonRpcConfig,
        test_case::test_case,
    };

    #[test_case("program-id", AccountIndex::ProgramId)]
    #[test_case("spl-token-mint", AccountIndex::SplTokenMint)]
    #[test_case("spl-token-owner", AccountIndex::SplTokenOwner)]
    fn verify_args_struct_by_command_run_with_account_indexes(
        arg_value: &str,
        expected_index: AccountIndex,
    ) {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_account_indexes = AccountSecondaryIndexes {
            keys: None,
            indexes: HashSet::from([expected_index]),
        };
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                account_indexes: expected_account_indexes.clone(),
                ..default_run_args.json_rpc_config.clone()
            },
            accounts_db_config: AccountsDbConfig {
                account_indexes: Some(expected_account_indexes.clone()),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--account-index", arg_value],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_account_indexes_multiple() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_account_indexes = AccountSecondaryIndexes {
            keys: None,
            indexes: HashSet::from([
                AccountIndex::ProgramId,
                AccountIndex::SplTokenMint,
                AccountIndex::SplTokenOwner,
            ]),
        };
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                account_indexes: expected_account_indexes.clone(),
                ..default_run_args.json_rpc_config.clone()
            },
            accounts_db_config: AccountsDbConfig {
                account_indexes: Some(expected_account_indexes.clone()),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--account-index",
                "program-id",
                "--account-index",
                "spl-token-mint",
                "--account-index",
                "spl-token-owner",
            ],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_account_index_include_key() {
        // single key
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let account_pubkey_1 = Pubkey::new_unique();
            let expected_account_indexes = AccountSecondaryIndexes {
                keys: Some(AccountSecondaryIndexesIncludeExclude {
                    exclude: false,
                    keys: HashSet::from([account_pubkey_1]),
                }),
                indexes: HashSet::from([AccountIndex::ProgramId]),
            };
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    account_indexes: expected_account_indexes.clone(),
                    ..default_run_args.json_rpc_config.clone()
                },
                accounts_db_config: AccountsDbConfig {
                    account_indexes: Some(expected_account_indexes.clone()),
                    ..default_run_args.accounts_db_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--account-index", // required by --account-index-include-key
                    "program-id",
                    "--account-index-include-key",
                    account_pubkey_1.to_string().as_str(),
                ],
                expected_args,
            );
        }

        // multiple keys
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let account_pubkey_1 = Pubkey::new_unique();
            let account_pubkey_2 = Pubkey::new_unique();
            let account_pubkey_3 = Pubkey::new_unique();
            let expected_account_indexes = AccountSecondaryIndexes {
                keys: Some(AccountSecondaryIndexesIncludeExclude {
                    exclude: false,
                    keys: HashSet::from([account_pubkey_1, account_pubkey_2, account_pubkey_3]),
                }),
                indexes: HashSet::from([AccountIndex::ProgramId]),
            };
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    account_indexes: expected_account_indexes.clone(),
                    ..default_run_args.json_rpc_config.clone()
                },
                accounts_db_config: AccountsDbConfig {
                    account_indexes: Some(expected_account_indexes.clone()),
                    ..default_run_args.accounts_db_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--account-index", // required by --account-index-include-key
                    "program-id",
                    "--account-index-include-key",
                    account_pubkey_1.to_string().as_str(),
                    "--account-index-include-key",
                    account_pubkey_2.to_string().as_str(),
                    "--account-index-include-key",
                    account_pubkey_3.to_string().as_str(),
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_account_index_exclude_key() {
        // single key
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let account_pubkey_1 = Pubkey::new_unique();
            let expected_account_indexes = AccountSecondaryIndexes {
                keys: Some(AccountSecondaryIndexesIncludeExclude {
                    exclude: true,
                    keys: HashSet::from([account_pubkey_1]),
                }),
                indexes: HashSet::from([AccountIndex::ProgramId]),
            };
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    account_indexes: expected_account_indexes.clone(),
                    ..default_run_args.json_rpc_config.clone()
                },
                accounts_db_config: AccountsDbConfig {
                    account_indexes: Some(expected_account_indexes.clone()),
                    ..default_run_args.accounts_db_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--account-index", // required by --account-index-exclude-key
                    "program-id",
                    "--account-index-exclude-key",
                    account_pubkey_1.to_string().as_str(),
                ],
                expected_args,
            );
        }

        // multiple keys
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let account_pubkey_1 = Pubkey::new_unique();
            let account_pubkey_2 = Pubkey::new_unique();
            let account_pubkey_3 = Pubkey::new_unique();
            let expected_account_indexes = AccountSecondaryIndexes {
                keys: Some(AccountSecondaryIndexesIncludeExclude {
                    exclude: true,
                    keys: HashSet::from([account_pubkey_1, account_pubkey_2, account_pubkey_3]),
                }),
                indexes: HashSet::from([AccountIndex::ProgramId]),
            };
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    account_indexes: expected_account_indexes.clone(),
                    ..default_run_args.json_rpc_config.clone()
                },
                accounts_db_config: AccountsDbConfig {
                    account_indexes: Some(expected_account_indexes.clone()),
                    ..default_run_args.accounts_db_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--account-index", // required by --account-index-exclude-key
                    "program-id",
                    "--account-index-exclude-key",
                    account_pubkey_1.to_string().as_str(),
                    "--account-index-exclude-key",
                    account_pubkey_2.to_string().as_str(),
                    "--account-index-exclude-key",
                    account_pubkey_3.to_string().as_str(),
                ],
                expected_args,
            );
        }
    }
}
