use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{App, AppSettings, Arg, ArgMatches, SubCommand, value_t},
    solana_clap_utils::input_validators::is_keypair,
    solana_keypair::read_keypair,
    solana_signer::Signer,
    std::{fs, path::Path},
};

const COMMAND: &str = "authorized-voter";

#[derive(Debug, PartialEq)]
#[cfg_attr(test, derive(Default))]
pub struct AuthorizedVoterAddArgs {
    pub authorized_voter_keypair: Option<String>,
}

impl FromClapArgMatches for AuthorizedVoterAddArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(AuthorizedVoterAddArgs {
            authorized_voter_keypair: value_t!(matches, "authorized_voter_keypair", String).ok(),
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
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
                            "Path to keypair of the authorized voter to add [default: read JSON \
                             keypair from stdin]",
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
                    "Note: the removal only applies to the currently running validator instance",
                ),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    match matches.subcommand() {
        ("add", Some(subcommand_matches)) => {
            let authorized_voter_add_args =
                AuthorizedVoterAddArgs::from_clap_arg_match(subcommand_matches)?;

            if let Some(authorized_voter_keypair) =
                authorized_voter_add_args.authorized_voter_keypair
            {
                let authorized_voter_keypair = fs::canonicalize(&authorized_voter_keypair)?;
                println!(
                    "Adding authorized voter path: {}",
                    authorized_voter_keypair.display()
                );

                let admin_client = admin_rpc_service::connect(ledger_path);
                admin_rpc_service::runtime().block_on(async move {
                    admin_client
                        .await?
                        .add_authorized_voter(authorized_voter_keypair.display().to_string())
                        .await
                })?;
            } else {
                let mut stdin = std::io::stdin();
                let authorized_voter_keypair = read_keypair(&mut stdin)?;
                println!(
                    "Adding authorized voter: {}",
                    authorized_voter_keypair.pubkey()
                );

                let admin_client = admin_rpc_service::connect(ledger_path);
                admin_rpc_service::runtime().block_on(async move {
                    admin_client
                        .await?
                        .add_authorized_voter_from_bytes(Vec::from(
                            authorized_voter_keypair.to_bytes(),
                        ))
                        .await
                })?;
            }
        }
        ("remove-all", _) => {
            let admin_client = admin_rpc_service::connect(ledger_path);
            admin_rpc_service::runtime().block_on(async move {
                admin_client.await?.remove_all_authorized_voters().await
            })?;
            println!("All authorized voters removed");
        }
        _ => unreachable!(),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, solana_keypair::Keypair};

    #[test]
    fn verify_args_struct_by_command_authorized_voter_add_default() {
        let app = command();
        let matches = app.get_matches_from(vec![COMMAND, "add"]);
        let subcommand_matches = matches.subcommand_matches("add").unwrap();
        let args = AuthorizedVoterAddArgs::from_clap_arg_match(subcommand_matches).unwrap();

        assert_eq!(args, AuthorizedVoterAddArgs::default());
    }

    #[test]
    fn verify_args_struct_by_command_authorized_voter_add_with_authorized_voter_keypair() {
        // generate a keypair
        let tmp_dir = tempfile::tempdir().unwrap();
        let file = tmp_dir.path().join("id.json");
        let keypair = Keypair::new();
        solana_keypair::write_keypair_file(&keypair, &file).unwrap();

        let app = command();
        let matches = app.get_matches_from(vec![COMMAND, "add", file.to_str().unwrap()]);
        let subcommand_matches = matches.subcommand_matches("add").unwrap();
        let args = AuthorizedVoterAddArgs::from_clap_arg_match(subcommand_matches).unwrap();

        assert_eq!(
            args,
            AuthorizedVoterAddArgs {
                authorized_voter_keypair: Some(file.to_str().unwrap().to_string()),
            }
        );
    }
}
