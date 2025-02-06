use {
    crate::{admin_rpc_service, cli::DefaultArgs, new_spinner_progress_bar, println_name_value},
    clap::{value_t_or_exit, App, Arg, ArgMatches, SubCommand},
    console::style,
    solana_clap_utils::{
        input_parsers::pubkey_of,
        input_validators::{is_parsable, is_pubkey_or_keypair, is_valid_percentage},
    },
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcLeaderScheduleConfig,
    solana_sdk::{
        clock::{Slot, DEFAULT_S_PER_SLOT},
        commitment_config::CommitmentConfig,
        pubkey::Pubkey,
    },
    std::{
        collections::VecDeque,
        path::Path,
        process::exit,
        time::{Duration, SystemTime},
    },
};

pub(crate) fn command(default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("wait-for-restart-window")
        .about("Monitor the validator for a good time to restart")
        .arg(
            Arg::with_name("min_idle_time")
                .long("min-idle-time")
                .takes_value(true)
                .validator(is_parsable::<usize>)
                .value_name("MINUTES")
                .default_value(&default_args.wait_for_restart_window_min_idle_time)
                .help(
                    "Minimum time that the validator should not be leader before restarting",
                ),
        )
        .arg(
            Arg::with_name("identity")
                .long("identity")
                .value_name("ADDRESS")
                .takes_value(true)
                .validator(is_pubkey_or_keypair)
                .help("Validator identity to monitor [default: your validator]"),
        )
        .arg(
            Arg::with_name("max_delinquent_stake")
                .long("max-delinquent-stake")
                .takes_value(true)
                .validator(is_valid_percentage)
                .default_value(&default_args.wait_for_restart_window_max_delinquent_stake)
                .value_name("PERCENT")
                .help("The maximum delinquent stake % permitted for a restart"),
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
        .after_help(
            "Note: If this command exits with a non-zero status then this not a good time for a restart",
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    let min_idle_time = value_t_or_exit!(matches, "min_idle_time", usize);
    let identity = pubkey_of(matches, "identity");
    let max_delinquent_stake = value_t_or_exit!(matches, "max_delinquent_stake", u8);
    let skip_new_snapshot_check = matches.is_present("skip_new_snapshot_check");
    let skip_health_check = matches.is_present("skip_health_check");

    wait_for_restart_window(
        ledger_path,
        identity,
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

pub fn wait_for_restart_window(
    ledger_path: &Path,
    identity: Option<Pubkey>,
    min_idle_time_in_minutes: usize,
    max_delinquency_percentage: u8,
    skip_new_snapshot_check: bool,
    skip_health_check: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let sleep_interval = Duration::from_secs(5);

    let min_idle_slots = (min_idle_time_in_minutes as f64 * 60. / DEFAULT_S_PER_SLOT) as Slot;

    let admin_client = admin_rpc_service::connect(ledger_path);
    let rpc_addr = admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.rpc_addr().await })
        .map_err(|err| format!("Unable to get validator RPC address: {err}"))?;

    let Some(rpc_client) = rpc_addr.map(RpcClient::new_socket) else {
        return Err("RPC not available".into());
    };

    let my_identity = rpc_client.get_identity()?;
    let identity = identity.unwrap_or(my_identity);
    let monitoring_another_validator = identity != my_identity;
    println_name_value("Identity:", &identity.to_string());
    println_name_value(
        "Minimum Idle Time:",
        &format!("{min_idle_slots} slots (~{min_idle_time_in_minutes} minutes)"),
    );

    println!("Maximum permitted delinquency: {max_delinquency_percentage}%");

    let mut current_epoch = None;
    let mut leader_schedule = VecDeque::new();
    let mut restart_snapshot = None;
    let mut upcoming_idle_windows = vec![]; // Vec<(starting slot, idle window length in slots)>

    let progress_bar = new_spinner_progress_bar();
    let monitor_start_time = SystemTime::now();

    let mut seen_incremential_snapshot = false;
    loop {
        let snapshot_slot_info = rpc_client.get_highest_snapshot_slot().ok();
        let snapshot_slot_info_has_incremential = snapshot_slot_info
            .as_ref()
            .map(|snapshot_slot_info| snapshot_slot_info.incremental.is_some())
            .unwrap_or_default();
        seen_incremential_snapshot |= snapshot_slot_info_has_incremential;

        let epoch_info = rpc_client.get_epoch_info_with_commitment(CommitmentConfig::processed())?;
        let healthy = skip_health_check || rpc_client.get_health().ok().is_some();
        let delinquent_stake_percentage = {
            let vote_accounts = rpc_client.get_vote_accounts()?;
            let current_stake: u64 = vote_accounts
                .current
                .iter()
                .map(|va| va.activated_stake)
                .sum();
            let delinquent_stake: u64 = vote_accounts
                .delinquent
                .iter()
                .map(|va| va.activated_stake)
                .sum();
            let total_stake = current_stake + delinquent_stake;
            delinquent_stake as f64 / total_stake as f64
        };

        if match current_epoch {
            None => true,
            Some(current_epoch) => current_epoch != epoch_info.epoch,
        } {
            progress_bar.set_message(format!(
                "Fetching leader schedule for epoch {}...",
                epoch_info.epoch
            ));
            let first_slot_in_epoch = epoch_info.absolute_slot - epoch_info.slot_index;
            leader_schedule = rpc_client
                .get_leader_schedule_with_config(
                    Some(first_slot_in_epoch),
                    RpcLeaderScheduleConfig {
                        identity: Some(identity.to_string()),
                        ..RpcLeaderScheduleConfig::default()
                    },
                )?
                .ok_or_else(|| {
                    format!("Unable to get leader schedule from slot {first_slot_in_epoch}")
                })?
                .get(&identity.to_string())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|slot_index| first_slot_in_epoch.saturating_add(slot_index as u64))
                .filter(|slot| *slot > epoch_info.absolute_slot)
                .collect::<VecDeque<_>>();

            upcoming_idle_windows.clear();
            {
                let mut leader_schedule = leader_schedule.clone();
                let mut max_idle_window = 0;

                let mut idle_window_start_slot = epoch_info.absolute_slot;
                while let Some(next_leader_slot) = leader_schedule.pop_front() {
                    let idle_window = next_leader_slot - idle_window_start_slot;
                    max_idle_window = max_idle_window.max(idle_window);
                    if idle_window > min_idle_slots {
                        upcoming_idle_windows.push((idle_window_start_slot, idle_window));
                    }
                    idle_window_start_slot = next_leader_slot;
                }
                if !leader_schedule.is_empty() && upcoming_idle_windows.is_empty() {
                    return Err(format!(
                        "Validator has no idle window of at least {} slots. Largest idle window \
                       for epoch {} is {} slots",
                        min_idle_slots, epoch_info.epoch, max_idle_window
                    )
                    .into());
                }
            }

            current_epoch = Some(epoch_info.epoch);
        }

        let status = {
            if !healthy {
                style("Node is unhealthy").red().to_string()
            } else {
                // Wait until a hole in the leader schedule before restarting the node
                let in_leader_schedule_hole = if epoch_info.slot_index + min_idle_slots
                    > epoch_info.slots_in_epoch
                {
                    Err("Current epoch is almost complete".to_string())
                } else {
                    while leader_schedule
                        .front()
                        .map(|slot| *slot < epoch_info.absolute_slot)
                        .unwrap_or(false)
                    {
                        leader_schedule.pop_front();
                    }
                    while upcoming_idle_windows
                        .first()
                        .map(|(slot, _)| *slot < epoch_info.absolute_slot)
                        .unwrap_or(false)
                    {
                        upcoming_idle_windows.pop();
                    }

                    match leader_schedule.front() {
                        None => {
                            Ok(()) // Validator has no leader slots
                        }
                        Some(next_leader_slot) => {
                            let idle_slots =
                                next_leader_slot.saturating_sub(epoch_info.absolute_slot);
                            if idle_slots >= min_idle_slots {
                                Ok(())
                            } else {
                                Err(match upcoming_idle_windows.first() {
                                    Some((starting_slot, length_in_slots)) => {
                                        format!(
                                            "Next idle window in {} slots, for {} slots",
                                            starting_slot.saturating_sub(epoch_info.absolute_slot),
                                            length_in_slots
                                        )
                                    }
                                    None => format!(
                                        "Validator will be leader soon. Next leader slot is \
                                       {next_leader_slot}"
                                    ),
                                })
                            }
                        }
                    }
                };

                match in_leader_schedule_hole {
                    Ok(_) => {
                        if skip_new_snapshot_check {
                            break; // Restart!
                        }
                        let snapshot_slot = snapshot_slot_info.map(|snapshot_slot_info| {
                            snapshot_slot_info
                                .incremental
                                .unwrap_or(snapshot_slot_info.full)
                        });
                        if restart_snapshot.is_none() {
                            restart_snapshot = snapshot_slot;
                        }
                        if restart_snapshot == snapshot_slot && !monitoring_another_validator {
                            "Waiting for a new snapshot".to_string()
                        } else if delinquent_stake_percentage
                            >= (max_delinquency_percentage as f64 / 100.)
                        {
                            style("Delinquency too high").red().to_string()
                        } else if seen_incremential_snapshot && !snapshot_slot_info_has_incremential
                        {
                            // Restarts using just a full snapshot will put the node significantly
                            // further behind than if an incremental snapshot is also used, as full
                            // snapshots are larger and take much longer to create.
                            //
                            // Therefore if the node just created a new full snapshot, wait a
                            // little longer until it creates the first incremental snapshot for
                            // the full snapshot.
                            "Waiting for incremental snapshot".to_string()
                        } else {
                            break; // Restart!
                        }
                    }
                    Err(why) => style(why).yellow().to_string(),
                }
            }
        };

        progress_bar.set_message(format!(
            "{} | Processed Slot: {} {} | {:.2}% delinquent stake | {}",
            {
                let elapsed =
                    chrono::Duration::from_std(monitor_start_time.elapsed().unwrap()).unwrap();

                format!(
                    "{:02}:{:02}:{:02}",
                    elapsed.num_hours(),
                    elapsed.num_minutes() % 60,
                    elapsed.num_seconds() % 60
                )
            },
            epoch_info.absolute_slot,
            if monitoring_another_validator {
                "".to_string()
            } else {
                format!(
                    "| Full Snapshot Slot: {} | Incremental Snapshot Slot: {}",
                    snapshot_slot_info
                        .as_ref()
                        .map(|snapshot_slot_info| snapshot_slot_info.full.to_string())
                        .unwrap_or_else(|| '-'.to_string()),
                    snapshot_slot_info
                        .as_ref()
                        .and_then(|snapshot_slot_info| snapshot_slot_info
                            .incremental
                            .map(|incremental| incremental.to_string()))
                        .unwrap_or_else(|| '-'.to_string()),
                )
            },
            delinquent_stake_percentage * 100.,
            status
        ));
        std::thread::sleep(sleep_interval);
    }
    drop(progress_bar);
    println!("{}", style("Ready to restart").green());
    Ok(())
}
