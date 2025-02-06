use {
    crate::{admin_rpc_service, cli::DefaultArgs, commands},
    clap::{value_t_or_exit, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::{is_parsable, is_valid_percentage},
    std::{path::Path, process::exit},
};

pub fn command(default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("exit")
        .about("Send an exit request to the validator")
        .arg(
            Arg::with_name("force")
                .short("f")
                .long("force")
                .takes_value(false)
                .help(
                    "Request the validator exit immediately instead of waiting for a restart window",
                ),
        )
        .arg(
            Arg::with_name("monitor")
                .short("m")
                .long("monitor")
                .takes_value(false)
                .help("Monitor the validator after sending the exit request"),
        )
        .arg(
            Arg::with_name("min_idle_time")
                .long("min-idle-time")
                .takes_value(true)
                .validator(is_parsable::<usize>)
                .value_name("MINUTES")
                .default_value(&default_args.exit_min_idle_time)
                .help(
                    "Minimum time that the validator should not be leader before restarting",
                ),
        )
        .arg(
            Arg::with_name("max_delinquent_stake")
                .long("max-delinquent-stake")
                .takes_value(true)
                .validator(is_valid_percentage)
                .default_value(&default_args.exit_max_delinquent_stake)
                .value_name("PERCENT")
                .help("The maximum delinquent stake % permitted for an exit"),
        )
        .arg(
            Arg::with_name("skip_new_snapshot_check")
                .long("skip-new-snapshot-check")
                .help("Skip check for a new snapshot"),
        )
        .arg(
            Arg::with_name("skip_health_check")
                .long("skip-health-check")
                .help("Skip health check"),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    let min_idle_time = value_t_or_exit!(matches, "min_idle_time", usize);
    let force = matches.is_present("force");
    let monitor = matches.is_present("monitor");
    let skip_new_snapshot_check = matches.is_present("skip_new_snapshot_check");
    let skip_health_check = matches.is_present("skip_health_check");
    let max_delinquent_stake = value_t_or_exit!(matches, "max_delinquent_stake", u8);

    if !force {
        commands::wait_for_restart_window::wait_for_restart_window(
            ledger_path,
            None,
            min_idle_time,
            max_delinquent_stake,
            skip_new_snapshot_check,
            skip_health_check,
        )
        .unwrap_or_else(|err| {
            println!("{err}");
            exit(1);
        });
    }

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.exit().await })
        .unwrap_or_else(|err| {
            println!("exit request failed: {err}");
            exit(1);
        });
    println!("Exit request sent");

    if monitor {
        commands::monitor::execute(matches, ledger_path);
    }
}
