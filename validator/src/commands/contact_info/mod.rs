use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{App, Arg, ArgMatches, SubCommand},
    std::{path::Path, process::exit},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
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

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    let output_mode = matches.value_of("output");
    let admin_client = admin_rpc_service::connect(ledger_path);
    let contact_info = admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.contact_info().await })
        .unwrap_or_else(|err| {
            eprintln!("Contact info query failed: {err}");
            exit(1);
        });
    if let Some(mode) = output_mode {
        match mode {
            "json" => println!("{}", serde_json::to_string_pretty(&contact_info).unwrap()),
            "json-compact" => print!("{}", serde_json::to_string(&contact_info).unwrap()),
            _ => unreachable!(),
        }
    } else {
        print!("{contact_info}");
    }
}
