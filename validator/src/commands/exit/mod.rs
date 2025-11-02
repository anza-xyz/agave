#[cfg(target_os = "linux")]
use std::{io, thread, time::Duration};
use {
    crate::{
        admin_rpc_service,
        commands::{monitor, wait_for_restart_window, Error, FromClapArgMatches, Result},
    },
    clap::{value_t_or_exit, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::{
        hidden_unless_forced,
        input_validators::{is_parsable, is_valid_percentage},
    },
    std::path::Path,
};

const COMMAND: &str = "exit";

const DEFAULT_MIN_IDLE_TIME: &str = "10";
const DEFAULT_MAX_DELINQUENT_STAKE: &str = "5";

#[derive(Clone, Debug, PartialEq)]
pub enum PostExitAction {
    // Run the agave-validator monitor command indefinitely
    Monitor,
    // Block until the exiting validator process has terminated
    Wait,
}

#[derive(Debug, PartialEq)]
pub struct ExitArgs {
    pub force: bool,
    pub post_exit_action: Option<PostExitAction>,
    pub min_idle_time: usize,
    pub max_delinquent_stake: u8,
    pub skip_new_snapshot_check: bool,
    pub skip_health_check: bool,
}

impl FromClapArgMatches for ExitArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let post_exit_action = if matches.is_present("monitor") {
            Some(PostExitAction::Monitor)
        } else if matches.is_present("no_wait_for_exit") {
            None
        } else {
            Some(PostExitAction::Wait)
        };

        // Deprecated in v3.0.0
        if matches.is_present("wait_for_exit") {
            eprintln!(
                "WARN: The --wait-for-exit flag has been deprecated, waiting for exit is now the \
                 default behavior"
            );
        }
        // Deprecated in v3.1.0
        if matches.is_present("monitor") {
            eprintln!(
                "WARN: The --monitor flag has been deprecated, use \"agave-validator monitor\" \
                 instead"
            );
        }

        Ok(ExitArgs {
            force: matches.is_present("force"),
            post_exit_action,
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
                    "Request the validator exit immediately instead of waiting for a restart \
                     window",
                ),
        )
        .arg(
            Arg::with_name("monitor")
                .short("m")
                .long("monitor")
                .takes_value(false)
                .requires("no_wait_for_exit")
                .hidden(hidden_unless_forced())
                .help("Monitor the validator after sending the exit request"),
        )
        .arg(
            Arg::with_name("wait_for_exit")
                .long("wait-for-exit")
                .conflicts_with("monitor")
                .hidden(hidden_unless_forced())
                .help("Wait for the validator to terminate after sending the exit request"),
        )
        .arg(
            Arg::with_name("no_wait_for_exit")
                .long("no-wait-for-exit")
                .takes_value(false)
                .conflicts_with("wait_for_exit")
                .help("Do not wait for the validator to terminate after sending the exit request"),
        )
        .arg(
            Arg::with_name("min_idle_time")
                .long("min-idle-time")
                .takes_value(true)
                .validator(is_parsable::<usize>)
                .value_name("MINUTES")
                .default_value(DEFAULT_MIN_IDLE_TIME)
                .help("Minimum time that the validator should not be leader before restarting"),
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

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let exit_args = ExitArgs::from_clap_arg_match(matches)?;

    if !exit_args.force {
        wait_for_restart_window::wait_for_restart_window(
            ledger_path,
            None,
            exit_args.min_idle_time,
            exit_args.max_delinquent_stake,
            exit_args.skip_new_snapshot_check,
            exit_args.skip_health_check,
        )?;
    }

    // Grab the pid from the process before initiating exit as the running
    // validator will be unable to respond after exit has returned.
    //
    // Additionally, only check the pid() RPC call result if it will be used.
    // In an upgrade scenario, it is possible that a binary that calls pid()
    // will be initiating exit against a process that doesn't support pid().
    const WAIT_FOR_EXIT_UNSUPPORTED_ERROR: &str = "remote process exit cannot be waited on. \
                                                   `--wait-for-exit` is not supported by the \
                                                   remote process";
    let post_exit_action = exit_args.post_exit_action.clone();
    let validator_pid = admin_rpc_service::runtime().block_on(async move {
        let admin_client = admin_rpc_service::connect(ledger_path).await?;
        let validator_pid = match post_exit_action {
            Some(PostExitAction::Wait) => admin_client
                .pid()
                .await
                .map_err(|_err| Error::Dynamic(WAIT_FOR_EXIT_UNSUPPORTED_ERROR.into()))?,
            _ => 0,
        };
        admin_client.exit().await?;

        Ok::<u32, Error>(validator_pid)
    })?;

    println!("Exit request sent");

    match exit_args.post_exit_action {
        None => Ok(()),
        Some(PostExitAction::Monitor) => monitor::execute(matches, ledger_path),
        Some(PostExitAction::Wait) => poll_until_pid_terminates(validator_pid),
    }?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn poll_until_pid_terminates(pid: u32) -> Result<()> {
    let pid = i32::try_from(pid)?;

    println!("Waiting for agave-validator process {pid} to terminate");
    loop {
        // From man kill(2)
        //
        // If sig is 0, then no signal is sent, but existence and permission
        // checks are still performed; this can be used to check for the
        // existence of a process ID or process group ID that the caller is
        // permitted to signal.
        let result = unsafe {
            libc::kill(pid, /*sig:*/ 0)
        };
        if result >= 0 {
            // Give the process some time to exit before checking again
            thread::sleep(Duration::from_millis(500));
        } else {
            let errno = io::Error::last_os_error()
                .raw_os_error()
                .ok_or(Error::Dynamic("unable to read raw os error".into()))?;
            match errno {
                libc::ESRCH => {
                    println!("Done, agave-validator process {pid} has terminated");
                    break;
                }
                libc::EINVAL => {
                    // An invalid signal was specified, we only pass sig=0 so
                    // this should not be possible
                    Err(Error::Dynamic(
                        format!("unexpected invalid signal error for kill({pid}, 0)").into(),
                    ))?;
                }
                libc::EPERM => {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))?;
                }
                unknown => {
                    Err(Error::Dynamic(
                        format!("unexpected errno for kill({pid}, 0): {unknown}").into(),
                    ))?;
                }
            }
        }
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn poll_until_pid_terminates(_pid: u32) -> Result<()> {
    Err(Error::Dynamic(
        "Unable to wait for agave-validator process termination on this platform".into(),
    ))
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
                post_exit_action: Some(PostExitAction::Wait),
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
    fn verify_args_struct_by_command_exit_with_post_exit_action() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--monitor", "--no-wait-for-exit"],
            ExitArgs {
                post_exit_action: Some(PostExitAction::Monitor),
                ..ExitArgs::default()
            },
        );

        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--no-wait-for-exit"],
            ExitArgs {
                post_exit_action: None,
                ..ExitArgs::default()
            },
        );

        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--wait-for-exit"],
            ExitArgs {
                post_exit_action: Some(PostExitAction::Wait),
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
