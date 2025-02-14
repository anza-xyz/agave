use {
    crate::{admin_rpc_service, cli::DefaultArgs, commands::FromClapArgMatches},
    clap::{App, Arg, ArgMatches, SubCommand},
    std::{path::Path, process::exit},
};

const COMMAND: &str = "contact-info";

#[derive(Debug, PartialEq)]
pub struct ContactInfoArgs {
    pub output: Option<String>,
}

impl FromClapArgMatches for ContactInfoArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Self {
        ContactInfoArgs {
            output: matches.value_of("output").map(String::from),
        }
    }
}

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
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

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    let contact_info_args = ContactInfoArgs::from_clap_arg_match(matches);

    let admin_client = admin_rpc_service::connect(ledger_path);
    let contact_info = admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.contact_info().await })
        .unwrap_or_else(|err| {
            eprintln!("Contact info query failed: {err}");
            exit(1);
        });
    if let Some(mode) = contact_info_args.output {
        match mode.as_str() {
            "json" => println!("{}", serde_json::to_string_pretty(&contact_info).unwrap()),
            "json-compact" => print!("{}", serde_json::to_string(&contact_info).unwrap()),
            _ => unreachable!(),
        }
    } else {
        print!("{contact_info}");
    }
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
            command(&DefaultArgs::default()),
            vec![COMMAND, "--output", "json"],
            ContactInfoArgs {
                output: Some("json".to_string()),
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_contact_info_output_json_compact() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND, "--output", "json-compact"],
            ContactInfoArgs {
                output: Some("json-compact".to_string()),
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_contact_info_output_default() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND],
            ContactInfoArgs { output: None },
        );
    }

    #[test]
    fn verify_args_struct_by_command_contact_info_output_invalid() {
        verify_args_struct_by_command_is_error::<ContactInfoArgs>(
            command(&DefaultArgs::default()),
            vec![COMMAND, "--output", "invalid_output_type"],
        );
    }
}
