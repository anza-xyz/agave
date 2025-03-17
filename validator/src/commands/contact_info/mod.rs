use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{App, Arg, ArgMatches, SubCommand},
    solana_cli_output::OutputFormat,
    std::path::Path,
};

const COMMAND: &str = "contact-info";

#[derive(Debug, PartialEq)]
pub struct ContactInfoArgs {
    pub output: OutputFormat,
}

impl FromClapArgMatches for ContactInfoArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(ContactInfoArgs {
            output: OutputFormat::from_matches(matches, "output", false),
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Display the validator's contact info")
        .arg(
            Arg::with_name("output")
                .long("output")
                .takes_value(true)
                .value_name("MODE")
                .possible_values(&["json", "json-compact"])
                .help("Output display mode"),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let contact_info_args = ContactInfoArgs::from_clap_arg_match(matches)?;

    let admin_client = admin_rpc_service::connect(ledger_path);
    let contact_info = admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.contact_info().await })?;

    println!(
        "{}",
        contact_info_args.output.formatted_string(&contact_info)
    );

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
    fn verify_args_struct_by_command_contact_info_output_json() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--output", "json"],
            ContactInfoArgs {
                output: OutputFormat::Json,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_contact_info_output_json_compact() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--output", "json-compact"],
            ContactInfoArgs {
                output: OutputFormat::JsonCompact,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_contact_info_output_default() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND],
            ContactInfoArgs {
                output: OutputFormat::Display,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_contact_info_output_invalid() {
        verify_args_struct_by_command_is_error::<ContactInfoArgs>(
            command(),
            vec![COMMAND, "--output", "invalid_output_type"],
        );
    }
}
