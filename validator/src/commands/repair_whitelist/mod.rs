use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{values_t_or_exit, App, AppSettings, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::is_pubkey,
    solana_sdk::pubkey::Pubkey,
    std::{collections::HashSet, path::Path, process::exit},
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
                    "Note: repair protocol whitelist changes only apply to the currently running \
                     validator instance",
                ),
        )
        .subcommand(
            SubCommand::with_name("remove-all")
                .about("Clear the validator's repair protocol whitelist")
                .after_help(
                    "Note: repair protocol whitelist changes only apply to the currently running \
                     validator instance",
                ),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    match matches.subcommand() {
        ("get", Some(subcommand_matches)) => {
            let output_mode = subcommand_matches.value_of("output");
            let admin_client = admin_rpc_service::connect(ledger_path);
            let repair_whitelist = admin_rpc_service::runtime()
                .block_on(async move { admin_client.await?.repair_whitelist().await })
                .unwrap_or_else(|err| {
                    eprintln!("Repair whitelist query failed: {err}");
                    exit(1);
                });
            if let Some(mode) = output_mode {
                match mode {
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
            let whitelist = if subcommand_matches.is_present("whitelist") {
                let validators_set: HashSet<_> =
                    values_t_or_exit!(subcommand_matches, "whitelist", Pubkey)
                        .into_iter()
                        .collect();
                validators_set.into_iter().collect::<Vec<_>>()
            } else {
                return;
            };
            set_repair_whitelist(ledger_path, whitelist).unwrap_or_else(|err| {
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
