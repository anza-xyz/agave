use {
    crate::{admin_rpc_service, commands::FromClapArgMatches},
    clap::{value_t, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::is_keypair,
    solana_sdk::signature::{read_keypair, Signer},
    std::{fs, path::Path},
};

const COMMAND: &str = "set-identity";

#[derive(Debug, PartialEq)]
#[cfg_attr(test, derive(Default))]
pub struct SetIdentityArgs {
    pub identity: Option<String>,
    pub require_tower: bool,
}

impl FromClapArgMatches for SetIdentityArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self, String> {
        Ok(SetIdentityArgs {
            identity: value_t!(matches, "identity", String).ok(),
            require_tower: matches.is_present("require_tower"),
        })
    }
}
pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Set the validator identity")
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
            clap::Arg::with_name("require_tower")
                .long("require-tower")
                .takes_value(false)
                .help("Refuse to set the validator identity if saved tower state is not found"),
        )
        .after_help(
            "Note: the new identity only applies to the currently running validator instance",
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<(), String> {
    let SetIdentityArgs {
        identity,
        require_tower,
    } = SetIdentityArgs::from_clap_arg_match(matches)?;

    if let Some(identity_keypair) = identity {
        let identity_keypair = fs::canonicalize(&identity_keypair)
            .map_err(|err| format!("unable to access path {identity_keypair}: {err:?}"))?;

        println!(
            "New validator identity path: {}",
            identity_keypair.display()
        );

        let admin_client = admin_rpc_service::connect(ledger_path);
        admin_rpc_service::runtime()
            .block_on(async move {
                admin_client
                    .await?
                    .set_identity(identity_keypair.display().to_string(), require_tower)
                    .await
            })
            .map_err(|err| format!("set identity request failed: {err}"))
    } else {
        let mut stdin = std::io::stdin();
        let identity_keypair = read_keypair(&mut stdin)
            .map_err(|err| format!("unable to read json keypair from stdin: {err:?}"))?;

        println!("New validator identity: {}", identity_keypair.pubkey());

        let admin_client = admin_rpc_service::connect(ledger_path);
        admin_rpc_service::runtime()
            .block_on(async move {
                admin_client
                    .await?
                    .set_identity_from_bytes(Vec::from(identity_keypair.to_bytes()), require_tower)
                    .await
            })
            .map_err(|err| format!("set identity request failed: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::commands::tests::verify_args_struct_by_command,
        solana_sdk::signature::Keypair,
    };

    #[test]
    fn verify_args_struct_by_command_set_identity_default() {
        verify_args_struct_by_command(command(), vec![COMMAND], SetIdentityArgs::default());
    }

    #[test]
    fn verify_args_struct_by_command_set_identity_with_identity_file() {
        // generate a keypair
        let tmp_dir = tempfile::tempdir().unwrap();
        let file = tmp_dir.path().join("id.json");
        let keypair = Keypair::new();
        solana_sdk::signature::write_keypair_file(&keypair, &file).unwrap();

        verify_args_struct_by_command(
            command(),
            vec![COMMAND, file.to_str().unwrap()],
            SetIdentityArgs {
                identity: Some(file.to_str().unwrap().to_string()),
                ..SetIdentityArgs::default()
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_set_identity_with_require_tower() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--require-tower"],
            SetIdentityArgs {
                require_tower: true,
                ..SetIdentityArgs::default()
            },
        );
    }
}
