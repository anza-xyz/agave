use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::is_keypair,
    solana_keypair::{read_keypair, read_keypair_file},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::{fs, path::Path},
};

const COMMAND: &str = "set-identity-and-vote-account";

#[derive(Debug, PartialEq)]
#[cfg_attr(test, derive(Default))]
pub struct SetIdentityAndVoteAccountArgs {
    pub identity: Option<String>,
    pub vote_account: String,
    pub require_tower: bool,
}

impl FromClapArgMatches for SetIdentityAndVoteAccountArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(SetIdentityAndVoteAccountArgs {
            identity: value_t!(matches, "identity", String).ok(),
            vote_account: value_t!(matches, "vote_account", String).map_err(|e| {
                clap::Error::with_description(&e.to_string(), clap::ErrorKind::ValueValidation)
            })?,
            require_tower: matches.is_present("require_tower"),
        })
    }
}

/// Resolve a vote account argument to a Pubkey.
///
/// Accepts:
/// - A bs58-encoded pubkey string
/// - A path to a keypair file (the pubkey is extracted)
/// - A path to a file containing a bs58-encoded pubkey
fn resolve_vote_account_pubkey(value: &str) -> std::result::Result<Pubkey, String> {
    // Try parsing as a bs58 pubkey directly
    if let Ok(pubkey) = value.parse::<Pubkey>() {
        return Ok(pubkey);
    }

    // Try reading as a keypair file
    if let Ok(keypair) = read_keypair_file(value) {
        return Ok(keypair.pubkey());
    }

    // Try reading as a file containing a bs58 pubkey
    if let Ok(contents) = fs::read_to_string(value) {
        if let Ok(pubkey) = contents.trim().parse::<Pubkey>() {
            return Ok(pubkey);
        }
    }

    Err(format!(
        "Could not parse '{value}' as a pubkey, keypair file, or pubkey file"
    ))
}

/// Clap validator that accepts a pubkey string, keypair file, or pubkey file
fn is_pubkey_or_keypair_or_pubkey_file<T: AsRef<str> + std::fmt::Display>(
    string: T,
) -> std::result::Result<(), String> {
    resolve_vote_account_pubkey(string.as_ref()).map(|_| ())
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Set the validator identity and vote account")
        .arg(
            Arg::with_name("identity")
                .index(1)
                .value_name("KEYPAIR")
                .required(false)
                .takes_value(true)
                .validator(is_keypair)
                .help("Path to validator identity keypair [default: read JSON keypair from stdin]"),
        )
        .arg(
            Arg::with_name("vote_account")
                .long("vote-account")
                .value_name("ADDRESS")
                .required(true)
                .takes_value(true)
                .validator(is_pubkey_or_keypair_or_pubkey_file)
                .help(
                    "Vote account public key. Accepts a base58 pubkey, a path to a keypair file, \
                     or a path to a file containing a base58 pubkey",
                ),
        )
        .arg(
            clap::Arg::with_name("require_tower")
                .long("require-tower")
                .takes_value(false)
                .help("Refuse to set the validator identity if saved tower state is not found"),
        )
        .after_help(
            "Note: the new identity and vote account only apply to the currently running \
             validator instance",
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let SetIdentityAndVoteAccountArgs {
        identity,
        vote_account,
        require_tower,
    } = SetIdentityAndVoteAccountArgs::from_clap_arg_match(matches)?;

    let vote_account_pubkey = resolve_vote_account_pubkey(&vote_account)
        .map_err(|e| clap::Error::with_description(&e, clap::ErrorKind::ValueValidation))?;
    let vote_account_str = vote_account_pubkey.to_string();

    if let Some(identity_keypair) = identity {
        let identity_keypair = fs::canonicalize(&identity_keypair)?;

        println!(
            "New validator identity path: {}",
            identity_keypair.display()
        );
        println!("New vote account: {vote_account_str}");

        let admin_client = admin_rpc_service::connect(ledger_path);
        admin_rpc_service::runtime().block_on(async move {
            admin_client
                .await?
                .set_identity_and_vote_account(
                    identity_keypair.display().to_string(),
                    vote_account_str,
                    require_tower,
                )
                .await
        })?;
    } else {
        let mut stdin = std::io::stdin();
        let identity_keypair = read_keypair(&mut stdin)?;

        println!("New validator identity: {}", identity_keypair.pubkey());
        println!("New vote account: {vote_account_str}");

        let admin_client = admin_rpc_service::connect(ledger_path);
        admin_rpc_service::runtime().block_on(async move {
            admin_client
                .await?
                .set_identity_and_vote_account_from_bytes(
                    Vec::from(identity_keypair.to_bytes()),
                    vote_account_str,
                    require_tower,
                )
                .await
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::tests::verify_args_struct_by_command,
        solana_keypair::Keypair,
        solana_signer::Signer,
    };

    #[test]
    fn verify_args_struct_by_command_with_pubkey_string() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let file = tmp_dir.path().join("id.json");
        let keypair = Keypair::new();
        solana_keypair::write_keypair_file(&keypair, &file).unwrap();

        let vote_account = Keypair::new();
        let vote_account_str = vote_account.pubkey().to_string();

        verify_args_struct_by_command(
            command(),
            vec![
                COMMAND,
                file.to_str().unwrap(),
                "--vote-account",
                &vote_account_str,
            ],
            SetIdentityAndVoteAccountArgs {
                identity: Some(file.to_str().unwrap().to_string()),
                vote_account: vote_account_str.clone(),
                require_tower: false,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_with_keypair_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let id_file = tmp_dir.path().join("id.json");
        let keypair = Keypair::new();
        solana_keypair::write_keypair_file(&keypair, &id_file).unwrap();

        let vote_keypair = Keypair::new();
        let vote_file = tmp_dir.path().join("vote.json");
        solana_keypair::write_keypair_file(&vote_keypair, &vote_file).unwrap();

        verify_args_struct_by_command(
            command(),
            vec![
                COMMAND,
                id_file.to_str().unwrap(),
                "--vote-account",
                vote_file.to_str().unwrap(),
            ],
            SetIdentityAndVoteAccountArgs {
                identity: Some(id_file.to_str().unwrap().to_string()),
                vote_account: vote_file.to_str().unwrap().to_string(),
                require_tower: false,
            },
        );

        // Verify resolution extracts the correct pubkey
        let resolved = resolve_vote_account_pubkey(vote_file.to_str().unwrap()).unwrap();
        assert_eq!(resolved, vote_keypair.pubkey());
    }

    #[test]
    fn verify_args_struct_by_command_with_pubkey_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let id_file = tmp_dir.path().join("id.json");
        let keypair = Keypair::new();
        solana_keypair::write_keypair_file(&keypair, &id_file).unwrap();

        let vote_pubkey = Keypair::new().pubkey();
        let pubkey_file = tmp_dir.path().join("vote_pubkey.txt");
        fs::write(&pubkey_file, vote_pubkey.to_string()).unwrap();

        verify_args_struct_by_command(
            command(),
            vec![
                COMMAND,
                id_file.to_str().unwrap(),
                "--vote-account",
                pubkey_file.to_str().unwrap(),
            ],
            SetIdentityAndVoteAccountArgs {
                identity: Some(id_file.to_str().unwrap().to_string()),
                vote_account: pubkey_file.to_str().unwrap().to_string(),
                require_tower: false,
            },
        );

        // Verify resolution extracts the correct pubkey
        let resolved = resolve_vote_account_pubkey(pubkey_file.to_str().unwrap()).unwrap();
        assert_eq!(resolved, vote_pubkey);
    }

    #[test]
    fn verify_args_struct_by_command_with_require_tower() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let file = tmp_dir.path().join("id.json");
        let keypair = Keypair::new();
        solana_keypair::write_keypair_file(&keypair, &file).unwrap();

        let vote_account = Keypair::new();
        let vote_account_str = vote_account.pubkey().to_string();

        verify_args_struct_by_command(
            command(),
            vec![
                COMMAND,
                file.to_str().unwrap(),
                "--vote-account",
                &vote_account_str,
                "--require-tower",
            ],
            SetIdentityAndVoteAccountArgs {
                identity: Some(file.to_str().unwrap().to_string()),
                vote_account: vote_account_str.clone(),
                require_tower: true,
            },
        );
    }

    #[test]
    fn test_resolve_vote_account_pubkey_invalid() {
        assert!(resolve_vote_account_pubkey("not-a-pubkey-or-file").is_err());
    }
}
