use {
    crate::{admin_rpc_service, cli::DefaultArgs, commands::FromClapArgMatches},
    clap::{values_t_or_exit, App, AppSettings, Arg, ArgMatches, SubCommand},
    solana_clap_utils::{input_parsers::value_of, input_validators::is_pubkey},
    solana_sdk::pubkey::Pubkey,
    std::{collections::HashSet, path::Path, process::exit},
};

const COMMAND: &str = "repair-whitelist";

#[derive(Debug, PartialEq)]
pub struct RepairWhitelistGetArg {
    pub output: Option<String>,
}

impl FromClapArgMatches for RepairWhitelistGetArg {
    fn from_clap_arg_match(matches: &ArgMatches) -> Self {
        RepairWhitelistGetArg {
            output: value_of::<String>(matches, "output"),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct RepairWhitelistSetArg {
    pub whitelist: Vec<Pubkey>,
}

impl FromClapArgMatches for RepairWhitelistSetArg {
    fn from_clap_arg_match(matches: &ArgMatches) -> Self {
        let whitelist = if matches.is_present("whitelist") {
            let validators_set: HashSet<_> = values_t_or_exit!(matches, "whitelist", Pubkey)
                .into_iter()
                .collect();
            validators_set.into_iter().collect::<Vec<_>>()
        } else {
            Vec::default()
        };
        RepairWhitelistSetArg { whitelist }
    }
}

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name(COMMAND)
        .about("Manage the validator's repair protocol whitelist")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::InferSubcommands)
        .subcommand(
            SubCommand::with_name("get")
                .about("Display the validator's repair protocol whitelist")
                .arg(
                    Arg::with_name("output")
                        .long("output")
                        .takes_value(true)
                        .value_name("MODE")
                        .possible_values(&["json", "json-compact"])
                        .help("Output display mode"),
                ),
        )
        .subcommand(
            SubCommand::with_name("set")
                .about("Set the validator's repair protocol whitelist")
                .setting(AppSettings::ArgRequiredElseHelp)
                .arg(
                    Arg::with_name("whitelist")
                        .long("whitelist")
                        .validator(is_pubkey)
                        .value_name("VALIDATOR IDENTITY")
                        .multiple(true)
                        .takes_value(true)
                        .help("Set the validator's repair protocol whitelist"),
                )
                .after_help(
                    "Note: repair protocol whitelist changes only apply to the currently running validator instance",
                ),
        )
        .subcommand(
            SubCommand::with_name("remove-all")
                .about("Clear the validator's repair protocol whitelist")
                .after_help(
                    "Note: repair protocol whitelist changes only apply to the currently running validator instance",
                ),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    match matches.subcommand() {
        ("get", Some(subcommand_matches)) => {
            let arg = RepairWhitelistGetArg::from_clap_arg_match(subcommand_matches);

            let admin_client = admin_rpc_service::connect(ledger_path);
            let repair_whitelist = admin_rpc_service::runtime()
                .block_on(async move { admin_client.await?.repair_whitelist().await })
                .unwrap_or_else(|err| {
                    eprintln!("Repair whitelist query failed: {err}");
                    exit(1);
                });
            if let Some(mode) = arg.output {
                match mode.as_str() {
                    "json" => println!(
                        "{}",
                        serde_json::to_string_pretty(&repair_whitelist).unwrap()
                    ),
                    "json-compact" => {
                        print!("{}", serde_json::to_string(&repair_whitelist).unwrap())
                    }
                    _ => unreachable!(),
                }
            } else {
                print!("{repair_whitelist}");
            }
        }
        ("set", Some(subcommand_matches)) => {
            let arg = RepairWhitelistSetArg::from_clap_arg_match(subcommand_matches);
            if arg.whitelist.is_empty() {
                return;
            }

            set_repair_whitelist(ledger_path, arg.whitelist).unwrap_or_else(|err| {
                eprintln!("{err}");
                exit(1);
            });
        }
        ("remove-all", _) => {
            set_repair_whitelist(ledger_path, Vec::default()).unwrap_or_else(|err| {
                eprintln!("{err}");
                exit(1);
            });
        }
        _ => unreachable!(),
    }
}

fn set_repair_whitelist(
    ledger_path: &Path,
    whitelist: Vec<Pubkey>,
) -> Result<(), Box<dyn std::error::Error>> {
    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.set_repair_whitelist(whitelist).await })
        .map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("setRepairWhitelist request failed: {err}"),
            )
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, std::str::FromStr};

    #[test]
    fn verify_args_struct_by_command_repair_whitelist_get_default() {
        let default_args = DefaultArgs::default();
        let app = command(&default_args);
        let matches = app.get_matches_from(vec![COMMAND, "get"]);
        let subcommand_matches = matches.subcommand_matches("get").unwrap();
        let arg = RepairWhitelistGetArg::from_clap_arg_match(subcommand_matches);
        assert_eq!(arg, RepairWhitelistGetArg { output: None });
    }

    #[test]
    fn verify_args_struct_by_command_repair_whitelist_get_with_output() {
        let default_args = DefaultArgs::default();
        let app = command(&default_args);
        let matches = app.get_matches_from(vec![COMMAND, "get", "--output", "json"]);
        let subcommand_matches = matches.subcommand_matches("get").unwrap();
        let arg = RepairWhitelistGetArg::from_clap_arg_match(subcommand_matches);
        assert_eq!(
            arg,
            RepairWhitelistGetArg {
                output: Some("json".to_string())
            }
        );
    }

    #[test]
    fn verify_args_struct_by_command_repair_whitelist_set_default() {
        let default_args = DefaultArgs::default();
        let app = command(&default_args);
        let matches = app.get_matches_from_safe(vec![COMMAND, "set"]);
        // need --whitelist
        assert!(matches.is_err());
    }

    #[test]
    fn verify_args_struct_by_command_repair_whitelist_set_with_single_whitelist() {
        let default_args = DefaultArgs::default();
        let app = command(&default_args);
        let matches = app.get_matches_from(vec![
            COMMAND,
            "set",
            "--whitelist",
            "ch1do11111111111111111111111111111111111111",
        ]);
        let subcommand_matches = matches.subcommand_matches("set").unwrap();
        let arg = RepairWhitelistSetArg::from_clap_arg_match(subcommand_matches);
        assert_eq!(
            arg,
            RepairWhitelistSetArg {
                whitelist: vec![
                    Pubkey::from_str("ch1do11111111111111111111111111111111111111").unwrap(),
                ]
            }
        );
    }

    #[test]
    fn verify_args_struct_by_command_repair_whitelist_set_with_multiple_whitelist() {
        let default_args = DefaultArgs::default();
        let app = command(&default_args);
        let matches = app.get_matches_from(vec![
            COMMAND,
            "set",
            "--whitelist",
            "ch1do11111111111111111111111111111111111111",
            "--whitelist",
            "ch1do11111111111111111111111111111111111112",
        ]);
        let subcommand_matches = matches.subcommand_matches("set").unwrap();
        let arg = RepairWhitelistSetArg::from_clap_arg_match(subcommand_matches);
        assert_eq!(
            arg,
            RepairWhitelistSetArg {
                whitelist: vec![
                    Pubkey::from_str("ch1do11111111111111111111111111111111111111").unwrap(),
                    Pubkey::from_str("ch1do11111111111111111111111111111111111112").unwrap(),
                ]
            }
        );
    }
}
