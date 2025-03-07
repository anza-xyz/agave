use {
    crate::{admin_rpc_service, cli::DefaultArgs, commands::FromClapArgMatches},
    clap::{value_t, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::is_keypair,
    solana_sdk::signature::{read_keypair, Signer},
    std::{fs, path::Path},
};

const COMMAND: &str = "set-identity";

#[derive(Debug, PartialEq)]
pub struct SetIdentityArg {
    pub identity: Option<String>,
    pub require_tower: bool,
}

impl FromClapArgMatches for SetIdentityArg {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self, String> {
        Ok(SetIdentityArg {
            identity: value_t!(matches, "identity", String).ok(),
            require_tower: matches.is_present("require_tower"),
        })
    }
}
pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
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
    let set_identity_arg = SetIdentityArg::from_clap_arg_match(matches)?;
    let require_tower = set_identity_arg.require_tower;

    if let Some(identity_keypair) = set_identity_arg.identity {
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
    use {super::*, crate::commands::tests::verify_args_struct_by_command};

    #[test]
    fn verify_args_struct_by_command_set_identity_default() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND],
            SetIdentityArg {
                identity: None,
                require_tower: false,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_set_identity_with_identity_file() {
        // generate a keypair
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp_path = tmp_dir.path().join("id.json");
        let keypair_string = "[99,66,147,169,175,95,166,214,27,255,19,64,81,255,101,39,10,24,205,48,226,191,98,234,210,86,174,34,2,121,173,223,9,36,145,159,1,95,129,252,249,189,217,191,13,169,231,216,4,181,124,105,193,20,61,251,197,68,44,240,205,70,115,226]";
        std::fs::write(&tmp_path, keypair_string).unwrap();

        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND, tmp_path.to_str().unwrap()],
            SetIdentityArg {
                identity: Some(tmp_path.to_str().unwrap().to_string()),
                require_tower: false,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_set_identity_with_require_tower() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND, "--require-tower"],
            SetIdentityArg {
                identity: None,
                require_tower: true,
            },
        );
    }
}
