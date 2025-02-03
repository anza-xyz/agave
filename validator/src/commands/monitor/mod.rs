use {
    crate::{cli::DefaultArgs, dashboard::Dashboard},
    clap::{App, ArgMatches, SubCommand},
    std::{path::Path, process::exit, time::Duration},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("monitor").about("Monitor the validator")
}

pub fn execute(_matches: &ArgMatches, ledger_path: &Path) {
    monitor_validator(ledger_path);
}

pub fn monitor_validator(ledger_path: &Path) {
    let dashboard = Dashboard::new(ledger_path, None, None).unwrap_or_else(|err| {
        println!(
            "Error: Unable to connect to validator at {}: {:?}",
            ledger_path.display(),
            err,
        );
        exit(1);
    });
    dashboard.run(Duration::from_secs(2));
}
