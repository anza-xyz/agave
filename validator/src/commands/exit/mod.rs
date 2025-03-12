use {
    crate::{
        admin_rpc_service,
        commands::{monitor, wait_for_restart_window, FromClapArgMatches},
    },
    clap::{value_t_or_exit, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::{is_parsable, is_valid_percentage},
    std::path::Path,
};

const COMMAND: &str = "exit";

const DEFAULT_MIN_IDLE_TIME: &str = "10";
const DEFAULT_MAX_DELINQUENT_STAKE: &str = "5";

#[derive(Debug, PartialEq)]
pub struct ExitArgs {
    pub force: bool,
    pub monitor: bool,
    pub min_idle_time: usize,
    pub max_delinquent_stake: u8,
    pub skip_new_snapshot_check: bool,
    pub skip_health_check: bool,
}

impl FromClapArgMatches for ExitArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self, String> {
        Ok(ExitArgs {
            force: matches.is_present("force"),
            monitor: matches.is_present("monitor"),
            min_idle_time: value_t_or_exit!(matches, "min_idle_time", usize),
            max_delinquent_stake: value_t_or_exit!(matches, "max_delinquent_stake", u8),
            skip_new_snapshot_check: matches.is_present("skip_new_snapshot_check"),
            skip_health_check: matches.is_present("skip_health_check"),
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
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
                .default_value(DEFAULT_MIN_IDLE_TIME)
                .help(
                    "Minimum time that the validator should not be leader before restarting",
                ),
        )
        .arg(
            Arg::with_name("max_delinquent_stake")
                .long("max-delinquent-stake")
                .takes_value(true)
                .validator(is_valid_percentage)
                .default_value(DEFAULT_MAX_DELINQUENT_STAKE)
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

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<(), String> {
    let exit_args = ExitArgs::from_clap_arg_match(matches)?;

    if !exit_args.force {
        wait_for_restart_window::wait_for_restart_window(
            ledger_path,
            None,
            exit_args.min_idle_time,
            exit_args.max_delinquent_stake,
            exit_args.skip_new_snapshot_check,
            exit_args.skip_health_check,
        )
        .map_err(|err| format!("error waiting for restart window: {err}"))?;
    }

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.exit().await })
        .map_err(|err| format!("exit request failed: {err}"))?;
    println!("Exit request sent");

    if exit_args.monitor {
        monitor::execute(matches, ledger_path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, crate::commands::tests::verify_args_struct_by_command};

    impl Default for ExitArgs {
        fn default() -> Self {
            ExitArgs {
                min_idle_time: DEFAULT_MIN_IDLE_TIME
                    .parse()
                    .expect("invalid DEFAULT_MIN_IDLE_TIME"),
                max_delinquent_stake: DEFAULT_MAX_DELINQUENT_STAKE
                    .parse()
                    .expect("invalid DEFAULT_MAX_DELINQUENT_STAKE"),
                force: false,
                monitor: false,
                skip_new_snapshot_check: false,
                skip_health_check: false,
            }
        }
    }

    #[test]
    fn verify_args_struct_by_command_exit_default() {
        verify_args_struct_by_command(command(), vec![COMMAND], ExitArgs::default());
    }

    #[test]
    fn verify_args_struct_by_command_exit_with_force() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--force"],
            ExitArgs {
                force: true,
                ..ExitArgs::default()
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_exit_with_monitor() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--monitor"],
            ExitArgs {
                monitor: true,
                ..ExitArgs::default()
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_exit_with_min_idle_time() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--min-idle-time", "60"],
            ExitArgs {
                min_idle_time: 60,
                ..ExitArgs::default()
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_exit_with_max_delinquent_stake() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--max-delinquent-stake", "10"],
            ExitArgs {
                max_delinquent_stake: 10,
                ..ExitArgs::default()
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_exit_with_skip_new_snapshot_check() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--skip-new-snapshot-check"],
            ExitArgs {
                skip_new_snapshot_check: true,
                ..ExitArgs::default()
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_exit_with_skip_health_check() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--skip-health-check"],
            ExitArgs {
                skip_health_check: true,
                ..ExitArgs::default()
            },
        );
    }
}
