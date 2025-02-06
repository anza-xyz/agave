use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{value_t_or_exit, App, Arg, ArgMatches, SubCommand},
    std::{path::Path, process::exit},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("set-log-filter")
        .about("Adjust the validator log filter")
        .arg(
            Arg::with_name("filter")
                .takes_value(true)
                .index(1)
                .help("New filter using the same format as the RUST_LOG environment variable"),
        )
        .after_help("Note: the new filter only applies to the currently running validator instance")
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    let filter = value_t_or_exit!(matches, "filter", String);
    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.set_log_filter(filter).await })
        .unwrap_or_else(|err| {
            println!("set log filter failed: {err}");
            exit(1);
        });
}
