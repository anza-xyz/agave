use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{App, Arg, ArgMatches, SubCommand},
    std::{path::PathBuf, process::exit},
};

pub struct ContactInfoArg {
    pub output: Option<String>,
}

impl ContactInfoArg {
    pub fn new(matches: &ArgMatches) -> Self {
        ContactInfoArg {
            output: matches.value_of("output").map(String::from),
        }
    }
}

pub fn command<'a>(_default_args: &'a DefaultArgs) -> App<'a, 'a> {
    SubCommand::with_name("contact-info")
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

pub fn execute(matches: &ArgMatches, ledger_path: &PathBuf) {
    let contact_info_arg = ContactInfoArg::new(matches);

    let admin_client = admin_rpc_service::connect(&ledger_path);
    let contact_info = admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.contact_info().await })
        .unwrap_or_else(|err| {
            eprintln!("Contact info query failed: {err}");
            exit(1);
        });
    if let Some(mode) = contact_info_arg.output {
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
    use super::*;

    #[test]
    fn test_contact_info_arg() {
        let default_args = DefaultArgs::default();

        let app = command(&default_args);
        let matches = app.get_matches_from(vec!["contact-info", "--output", "json"]);
        let arg = ContactInfoArg::new(&matches);
        assert_eq!(arg.output, Some("json".to_string()));

        let app = command(&default_args);
        let matches = app.get_matches_from(vec!["contact-info", "--output", "json-compact"]);
        let arg = ContactInfoArg::new(&matches);
        assert_eq!(arg.output, Some("json-compact".to_string()));

        let app = command(&default_args);
        let result = app.get_matches_from_safe(vec!["contact-info", "--output", "xxx"]);
        assert!(result.is_err());
    }
}
