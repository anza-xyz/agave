use {
    crate::{cli::DefaultArgs, dashboard::Dashboard},
    clap::{App, ArgMatches, SubCommand},
    std::{
        path::{Path, PathBuf},
        process::exit,
        time::Duration,
    },
};

pub fn command<'a>(_default_args: &'a DefaultArgs) -> App<'a, 'a> {
    SubCommand::with_name("monitor").about("Monitor the validator")
}

pub fn execute(_matches: &ArgMatches, ledger_path: &PathBuf) {
    monitor_validator(&ledger_path);
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
