use {
    crate::{admin_rpc_service, commands::FromClapArgMatches},
    clap::{values_t_or_exit, App, AppSettings, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::is_pubkey,
    solana_cli_output::OutputFormat,
    solana_sdk::pubkey::Pubkey,
    std::{collections::HashSet, path::Path},
};

pub const COMMAND: &str = "repair-whitelist";

#[derive(Debug, PartialEq)]
pub struct RepairWhitelistGetArgs {
    pub output: OutputFormat,
}

impl FromClapArgMatches for RepairWhitelistGetArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self, String> {
        Ok(RepairWhitelistGetArgs {
            output: OutputFormat::from_matches(matches, "output", false),
        })
    }
}

#[derive(Debug, PartialEq)]
pub struct RepairWhitelistSetArgs {
    pub whitelist: Vec<Pubkey>,
}

impl FromClapArgMatches for RepairWhitelistSetArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self, String> {
        let whitelist = if matches.is_present("whitelist") {
            let validators_set: HashSet<_> = values_t_or_exit!(matches, "whitelist", Pubkey)
                .into_iter()
                .collect();
            validators_set.into_iter().collect::<Vec<_>>()
        } else {
            return Ok(RepairWhitelistSetArgs { whitelist: vec![] });
        };

        Ok(RepairWhitelistSetArgs { whitelist })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
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

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<(), String> {
    match matches.subcommand() {
        ("get", Some(subcommand_matches)) => {
            let repair_whitelist_get_args =
                RepairWhitelistGetArgs::from_clap_arg_match(subcommand_matches)?;

            let admin_client = admin_rpc_service::connect(ledger_path);
            let repair_whitelist = admin_rpc_service::runtime()
                .block_on(async move { admin_client.await?.repair_whitelist().await })
                .map_err(|err| format!("get repair whitelist request failed: {err}"))?;

            println!(
                "{}",
                repair_whitelist_get_args
                    .output
                    .formatted_string(&repair_whitelist)
            );
        }
        ("set", Some(subcommand_matches)) => {
            let RepairWhitelistSetArgs { whitelist } =
                RepairWhitelistSetArgs::from_clap_arg_match(subcommand_matches)?;

            if whitelist.is_empty() {
                return Ok(());
            }

            set_repair_whitelist(ledger_path, whitelist)?;
        }
        ("remove-all", _) => {
            set_repair_whitelist(ledger_path, Vec::default())?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn set_repair_whitelist(ledger_path: &Path, whitelist: Vec<Pubkey>) -> Result<(), String> {
    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.set_repair_whitelist(whitelist).await })
        .map_err(|err| format!("set repair whitelist request failed: {err}"))
}

#[cfg(test)]
mod tests {
    use {super::*, std::str::FromStr};

    #[test]
    fn verify_args_struct_by_command_repair_whitelist_get_default() {
        let app = command();
        let matches = app.get_matches_from(vec![COMMAND, "get"]);
        let subcommand_matches = matches.subcommand_matches("get").unwrap();
        let args = RepairWhitelistGetArgs::from_clap_arg_match(subcommand_matches).unwrap();
        assert_eq!(
            args,
            RepairWhitelistGetArgs {
                output: OutputFormat::Display
            }
        );
    }

    #[test]
    fn verify_args_struct_by_command_repair_whitelist_get_with_output() {
        let app = command();
        let matches = app.get_matches_from(vec![COMMAND, "get", "--output", "json"]);
        let subcommand_matches = matches.subcommand_matches("get").unwrap();
        let args = RepairWhitelistGetArgs::from_clap_arg_match(subcommand_matches).unwrap();
        assert_eq!(
            args,
            RepairWhitelistGetArgs {
                output: OutputFormat::Json
            }
        );
    }

    #[test]
    fn verify_args_struct_by_command_repair_whitelist_set_with_single_whitelist() {
        let app = command();
        let matches = app.get_matches_from(vec![
            COMMAND,
            "set",
            "--whitelist",
            "ch1do11111111111111111111111111111111111111",
        ]);
        let subcommand_matches = matches.subcommand_matches("set").unwrap();
        let args = RepairWhitelistSetArgs::from_clap_arg_match(subcommand_matches).unwrap();
        assert_eq!(
            args,
            RepairWhitelistSetArgs {
                whitelist: vec![
                    Pubkey::from_str("ch1do11111111111111111111111111111111111111").unwrap(),
                ]
            }
        );
    }

    #[test]
    fn verify_args_struct_by_command_repair_whitelist_set_with_multiple_whitelist() {
        let app = command();
        let matches = app.get_matches_from(vec![
            COMMAND,
            "set",
            "--whitelist",
            "ch1do11111111111111111111111111111111111111",
            "--whitelist",
            "ch1do11111111111111111111111111111111111112",
        ]);
        let subcommand_matches = matches.subcommand_matches("set").unwrap();
        let mut args = RepairWhitelistSetArgs::from_clap_arg_match(subcommand_matches).unwrap();
        args.whitelist.sort(); // the order of the whitelist is not guaranteed. sort it before asserting
        assert_eq!(
            args,
            RepairWhitelistSetArgs {
                whitelist: vec![
                    Pubkey::from_str("ch1do11111111111111111111111111111111111111").unwrap(),
                    Pubkey::from_str("ch1do11111111111111111111111111111111111112").unwrap(),
                ]
            }
        );
    }
}
