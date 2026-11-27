use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{App, Arg, ArgMatches, SubCommand},
    std::path::Path,
};

const COMMAND: &str = "shred-receiver-addresses";

#[derive(Debug, PartialEq)]
pub struct ShredReceiverAddressesArgs {
    pub path: String,
}

impl FromClapArgMatches for ShredReceiverAddressesArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(ShredReceiverAddressesArgs {
            path: matches
                .value_of("path")
                .expect("path is required")
                .to_string(),
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Replaces the shred receiver addresses with those listed in a file.")
        .arg(
            Arg::with_name("path")
                .value_name("PATH")
                .takes_value(true)
                .required(true)
                .help(
                    "Path to a file in the --shred-receiver-address-file format, as the running \
                     validator sees it. An empty file removes every receiver.",
                ),
        )
        .after_help(
            "Note: the new list only applies to the currently running validator instance. A file \
             that does not load leaves the current list in place.",
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let args = ShredReceiverAddressesArgs::from_clap_arg_match(matches)?;

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime().block_on(async move {
        admin_client
            .await?
            .set_shred_receiver_addresses(args.path)
            .await
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::tests::{
            verify_args_struct_by_command, verify_args_struct_by_command_is_error,
        },
    };

    #[test]
    fn verify_args_struct_by_command_shred_receiver_addresses_default() {
        verify_args_struct_by_command_is_error::<ShredReceiverAddressesArgs>(
            command(),
            vec![COMMAND],
        );
    }

    #[test]
    fn verify_args_struct_by_command_shred_receiver_addresses_path() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "receivers.txt"],
            ShredReceiverAddressesArgs {
                path: "receivers.txt".to_string(),
            },
        );
    }
}
