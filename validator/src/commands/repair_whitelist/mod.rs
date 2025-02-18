use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{values_t_or_exit, App, AppSettings, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::is_pubkey,
    solana_cli_output::OutputFormat,
    solana_sdk::pubkey::Pubkey,
    std::{collections::HashSet, path::Path},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("repair-whitelist")
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
            let output = OutputFormat::from_matches(subcommand_matches, "output", false);
            let admin_client = admin_rpc_service::connect(ledger_path);
            let repair_whitelist = admin_rpc_service::runtime()
                .block_on(async move { admin_client.await?.repair_whitelist().await })
                .map_err(|err| format!("get repair whitelist request failed: {err}"))?;

            println!("{}", output.formatted_string(&repair_whitelist));
        }
        ("set", Some(subcommand_matches)) => {
            let whitelist = if subcommand_matches.is_present("whitelist") {
                let validators_set: HashSet<_> =
                    values_t_or_exit!(subcommand_matches, "whitelist", Pubkey)
                        .into_iter()
                        .collect();
                validators_set.into_iter().collect::<Vec<_>>()
            } else {
                return Ok(());
            };
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
