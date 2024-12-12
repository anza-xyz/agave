use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{value_t, App, AppSettings, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::is_keypair,
    solana_sdk::signature::{read_keypair, Signer},
    std::{fs, path::PathBuf, process::exit},
};

pub fn command<'a>(_default_args: &'a DefaultArgs) -> App<'a, 'a> {
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
                            "Path to keypair of the authorized voter to add [default: \
                       read JSON keypair from stdin]",
                        ),
                )
                .after_help(
                    "Note: the new authorized voter only applies to the currently running \
               validator instance",
                ),
        )
        .subcommand(
            SubCommand::with_name("remove-all")
                .about("Remove all authorized voters")
                .after_help(
                    "Note: the removal only applies to the currently running validator \
               instance",
                ),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &PathBuf) {
    match matches.subcommand() {
        ("add", Some(subcommand_matches)) => {
            if let Ok(authorized_voter_keypair) =
                value_t!(subcommand_matches, "authorized_voter_keypair", String)
            {
                let authorized_voter_keypair = fs::canonicalize(&authorized_voter_keypair)
                    .unwrap_or_else(|err| {
                        println!("Unable to access path: {authorized_voter_keypair}: {err:?}");
                        exit(1);
                    });
                println!(
                    "Adding authorized voter path: {}",
                    authorized_voter_keypair.display()
                );

                let admin_client = admin_rpc_service::connect(&ledger_path);
                admin_rpc_service::runtime()
                    .block_on(async move {
                        admin_client
                            .await?
                            .add_authorized_voter(authorized_voter_keypair.display().to_string())
                            .await
                    })
                    .unwrap_or_else(|err| {
                        println!("addAuthorizedVoter request failed: {err}");
                        exit(1);
                    });
            } else {
                let mut stdin = std::io::stdin();
                let authorized_voter_keypair = read_keypair(&mut stdin).unwrap_or_else(|err| {
                    println!("Unable to read JSON keypair from stdin: {err:?}");
                    exit(1);
                });
                println!(
                    "Adding authorized voter: {}",
                    authorized_voter_keypair.pubkey()
                );

                let admin_client = admin_rpc_service::connect(&ledger_path);
                admin_rpc_service::runtime()
                    .block_on(async move {
                        admin_client
                            .await?
                            .add_authorized_voter_from_bytes(Vec::from(
                                authorized_voter_keypair.to_bytes(),
                            ))
                            .await
                    })
                    .unwrap_or_else(|err| {
                        println!("addAuthorizedVoterFromBytes request failed: {err}");
                        exit(1);
                    });
            }

            return;
        }
        ("remove-all", _) => {
            let admin_client = admin_rpc_service::connect(&ledger_path);
            admin_rpc_service::runtime()
                .block_on(async move { admin_client.await?.remove_all_authorized_voters().await })
                .unwrap_or_else(|err| {
                    println!("removeAllAuthorizedVoters request failed: {err}");
                    exit(1);
                });
            println!("All authorized voters removed");
            return;
        }
        _ => unreachable!(),
    }
}
