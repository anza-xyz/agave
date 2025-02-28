use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{value_t, App, AppSettings, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::is_keypair,
    solana_sdk::signature::{read_keypair, Signer},
    std::{fs, path::Path},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("authorized-voter")
        .about("Adjust the validator authorized voters")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::InferSubcommands)
        .subcommand(
            SubCommand::with_name("add")
                .about("Add an authorized voter")
                .arg(
                    Arg::with_name("authorized_voter_keypair")
                        .index(1)
                        .value_name("KEYPAIR")
                        .required(false)
                        .takes_value(true)
                        .validator(is_keypair)
                        .help(
                            "Path to keypair of the authorized voter to add [default: read JSON keypair from stdin]",
                        ),
                )
                .after_help(
                    "Note: the new authorized voter only applies to the currently running validator instance",
                ),
        )
        .subcommand(
            SubCommand::with_name("remove-all")
                .about("Remove all authorized voters")
                .after_help(
                    "Note: the removal only applies to the currently running validator instance",
                ),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<(), String> {
    match matches.subcommand() {
        ("add", Some(subcommand_matches)) => {
            if let Ok(authorized_voter_keypair) =
                value_t!(subcommand_matches, "authorized_voter_keypair", String)
            {
                let authorized_voter_keypair = fs::canonicalize(&authorized_voter_keypair)
                    .map_err(|err| {
                        format!("unable to access path {authorized_voter_keypair}: {err:?}")
                    })?;

                println!(
                    "Adding authorized voter path: {}",
                    authorized_voter_keypair.display()
                );

                let admin_client = admin_rpc_service::connect(ledger_path);
                admin_rpc_service::runtime()
                    .block_on(async move {
                        admin_client
                            .await?
                            .add_authorized_voter(authorized_voter_keypair.display().to_string())
                            .await
                    })
                    .map_err(|err| format!("add authorized voter request failed: {err}"))?;
            } else {
                let mut stdin = std::io::stdin();
                let authorized_voter_keypair = read_keypair(&mut stdin)
                    .map_err(|err| format!("unable to read json keypair from stdin: {err:?}"))?;
                println!(
                    "Adding authorized voter: {}",
                    authorized_voter_keypair.pubkey()
                );

                let admin_client = admin_rpc_service::connect(ledger_path);
                admin_rpc_service::runtime()
                    .block_on(async move {
                        admin_client
                            .await?
                            .add_authorized_voter_from_bytes(Vec::from(
                                authorized_voter_keypair.to_bytes(),
                            ))
                            .await
                    })
                    .map_err(|err| format!("add authorized voter request failed: {err}"))?;
            }
        }
        ("remove-all", _) => {
            let admin_client = admin_rpc_service::connect(ledger_path);
            admin_rpc_service::runtime()
                .block_on(async move { admin_client.await?.remove_all_authorized_voters().await })
                .map_err(|err| format!("remove all authorized voters request failed: {err}"))?;
            println!("All authorized voters removed");
        }
        _ => unreachable!(),
    }

    Ok(())
}
