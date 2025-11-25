//! A command-line executable for monitoring the health of a cluster
#![allow(clippy::arithmetic_side_effects)]

use {
    clap::{crate_description, crate_name, value_t, value_t_or_exit, values_t, App, Arg},
    log::*,
    solana_clap_utils::{
        hidden_unless_forced,
        input_parsers::pubkeys_of,
        input_validators::{is_parsable, is_pubkey_or_keypair, is_url, is_valid_percentage},
    },
    solana_cli_output::display::format_labeled_address,
    solana_hash::Hash,
    solana_metrics::{datapoint_error, datapoint_info},
    solana_native_token::{sol_str_to_lamports, Sol},
    solana_notifier::{NotificationType, Notifier},
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::{client_error, response::RpcVoteAccountStatus},
    std::{
        collections::HashMap,
        error,
        thread::sleep,
        time::{Duration, Instant},
    },
};

struct Config {
    address_labels: HashMap<String, String>,
    ignore_http_bad_gateway: bool,
    interval: Duration,
    json_rpc_urls: Vec<String>,
    rpc_timeout: Duration,
    minimum_validator_identity_balance: u64,
    monitor_active_stake: bool,
    active_stake_alert_threshold: u8,
    unhealthy_threshold: usize,
    validator_identity_pubkeys: Vec<Pubkey>,
    name_suffix: String,
    acceptable_slot_range: u64,
}

fn get_config() -> Config {
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .after_help(
            "ADDITIONAL HELP:
        To receive a Slack, Discord, PagerDuty and/or Telegram notification on sanity failure,
        define environment variables before running `agave-watchtower`:

        export SLACK_WEBHOOK=...
        export DISCORD_WEBHOOK=...

        Telegram requires the following two variables:

        export TELEGRAM_BOT_TOKEN=...
        export TELEGRAM_CHAT_ID=...

        PagerDuty requires an Integration Key from the Events API v2 (Add this integration to your \
             PagerDuty service to get this)

        export PAGERDUTY_INTEGRATION_KEY=...

        To receive a Twilio SMS notification on failure, having a Twilio account,
        and a sending number owned by that account,
        define environment variable before running `agave-watchtower`:

        export \
             TWILIO_CONFIG='ACCOUNT=<account>,TOKEN=<securityToken>,TO=<receivingNumber>,\
             FROM=<sendingNumber>'",
        )
        .arg({
            let arg = Arg::with_name("config_file")
                .short("C")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .global(true)
                .help("Configuration file to use");
            if let Some(ref config_file) = *solana_cli_config::CONFIG_FILE {
                arg.default_value(config_file)
            } else {
                arg
            }
        })
        .arg(
            Arg::with_name("json_rpc_url")
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .validator(is_url)
                .help("JSON RPC URL for the cluster (conflicts with --urls)"),
        )
        .arg(
            Arg::with_name("json_rpc_urls")
                .long("urls")
                .value_name("URL")
                .takes_value(true)
                .validator(is_url)
                .multiple(true)
                .number_of_values(3)
                .conflicts_with("json_rpc_url")
                .help(
                    "JSON RPC URLs for the cluster (takes exactly 3 values, conflicts with --url)",
                ),
        )
        .arg(
            Arg::with_name("rpc_timeout")
                .long("rpc-timeout")
                .value_name("SECONDS")
                .takes_value(true)
                .default_value("30")
                .help("Timeout value for RPC requests"),
        )
        .arg(
            Arg::with_name("interval")
                .long("interval")
                .value_name("SECONDS")
                .takes_value(true)
                .default_value("60")
                .help("Wait interval seconds between checking the cluster"),
        )
        .arg(
            Arg::with_name("unhealthy_threshold")
                .long("unhealthy-threshold")
                .value_name("COUNT")
                .takes_value(true)
                .default_value("1")
                .help("How many consecutive failures must occur to trigger a notification"),
        )
        .arg(
            Arg::with_name("validator_identities")
                .long("validator-identity")
                .value_name("VALIDATOR IDENTITY PUBKEY")
                .takes_value(true)
                .validator(is_pubkey_or_keypair)
                .multiple(true)
                .help("Validator identities to monitor for delinquency"),
        )
        .arg(
            Arg::with_name("minimum_validator_identity_balance")
                .long("minimum-validator-identity-balance")
                .value_name("SOL")
                .takes_value(true)
                .default_value("10")
                .validator(is_parsable::<f64>)
                .help("Alert when the validator identity balance is less than this amount of SOL"),
        )
        .arg(
            // Deprecated parameter, now always enabled
            Arg::with_name("no_duplicate_notifications")
                .long("no-duplicate-notifications")
                .hidden(hidden_unless_forced()),
        )
        .arg(
            Arg::with_name("monitor_active_stake")
                .long("monitor-active-stake")
                .takes_value(false)
                .help(
                    "Alert when the current stake for the cluster drops below the amount \
                     specified by --active-stake-alert-threshold",
                ),
        )
        .arg(
            Arg::with_name("active_stake_alert_threshold")
                .long("active-stake-alert-threshold")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .validator(is_valid_percentage)
                .default_value("80")
                .help("Alert when the current stake for the cluster drops below this value"),
        )
        .arg(
            Arg::with_name("ignore_http_bad_gateway")
                .long("ignore-http-bad-gateway")
                .takes_value(false)
                .help(
                    "Ignore HTTP 502 Bad Gateway errors from the JSON RPC URL. This flag can help \
                     reduce false positives, at the expense of no alerting should a Bad Gateway \
                     error be a side effect of the real problem",
                ),
        )
        .arg(
            Arg::with_name("name_suffix")
                .long("name-suffix")
                .value_name("SUFFIX")
                .takes_value(true)
                .default_value("")
                .help("Add this string into all notification messages after \"agave-watchtower\""),
        )
        .arg(
            Arg::with_name("acceptable_slot_range")
                .long("acceptable-slot-range")
                .value_name("RANGE")
                .takes_value(true)
                .default_value("50")
                .validator(is_parsable::<u64>)
                .help("Acceptable range of slots for endpoints, checked at watchtower startup"),
        )
        .get_matches();

    let config = if let Some(config_file) = matches.value_of("config_file") {
        solana_cli_config::Config::load(config_file).unwrap_or_default()
    } else {
        solana_cli_config::Config::default()
    };

    let interval = Duration::from_secs(value_t_or_exit!(matches, "interval", u64));
    let unhealthy_threshold = value_t_or_exit!(matches, "unhealthy_threshold", usize);
    let minimum_validator_identity_balance = matches
        .value_of("minimum_validator_identity_balance")
        .and_then(sol_str_to_lamports)
        .unwrap();
    let json_rpc_urls = values_t!(matches, "json_rpc_urls", String).unwrap_or_else(|_| {
        vec![value_t!(matches, "json_rpc_url", String).unwrap_or_else(|_| config.json_rpc_url)]
    });
    let rpc_timeout = value_t_or_exit!(matches, "rpc_timeout", u64);
    let rpc_timeout = Duration::from_secs(rpc_timeout);
    let validator_identity_pubkeys: Vec<_> = pubkeys_of(&matches, "validator_identities")
        .unwrap_or_default()
        .into_iter()
        .collect();

    let monitor_active_stake = matches.is_present("monitor_active_stake");
    let active_stake_alert_threshold =
        value_t_or_exit!(matches, "active_stake_alert_threshold", u8);
    let ignore_http_bad_gateway = matches.is_present("ignore_http_bad_gateway");

    let name_suffix = value_t_or_exit!(matches, "name_suffix", String);

    let acceptable_slot_range = value_t_or_exit!(matches, "acceptable_slot_range", u64);

    let config = Config {
        address_labels: config.address_labels,
        ignore_http_bad_gateway,
        interval,
        json_rpc_urls,
        rpc_timeout,
        minimum_validator_identity_balance,
        monitor_active_stake,
        active_stake_alert_threshold,
        unhealthy_threshold,
        validator_identity_pubkeys,
        name_suffix,
        acceptable_slot_range,
    };

    info!("RPC URLs: {:?}", config.json_rpc_urls);
    info!(
        "Monitored validators: {:?}",
        config.validator_identity_pubkeys
    );
    config
}

fn get_cluster_info(
    config: &Config,
    rpc_client: &RpcClient,
) -> client_error::Result<(u64, Hash, RpcVoteAccountStatus, HashMap<Pubkey, u64>)> {
    let transaction_count = rpc_client.get_transaction_count()?;
    let recent_blockhash = rpc_client.get_latest_blockhash()?;
    let vote_accounts = rpc_client.get_vote_accounts()?;

    let mut validator_balances = HashMap::new();
    for validator_identity in &config.validator_identity_pubkeys {
        validator_balances.insert(
            *validator_identity,
            rpc_client.get_balance(validator_identity)?,
        );
    }

    Ok((
        transaction_count,
        recent_blockhash,
        vote_accounts,
        validator_balances,
    ))
}

struct EndpointData {
    rpc_client: RpcClient,
    last_transaction_count: u64,
    last_recent_blockhash: Hash,
}

fn query_endpoint(
    config: &Config,
    endpoint: &mut EndpointData,
) -> client_error::Result<Option<(&'static str, String)>> {
    info!("Querying {}", endpoint.rpc_client.url());

    match get_cluster_info(config, &endpoint.rpc_client) {
        Ok((transaction_count, recent_blockhash, vote_accounts, validator_balances)) => {
            info!("Current transaction count: {transaction_count}");
            info!("Recent blockhash: {recent_blockhash}");
            info!("Current validator count: {}", vote_accounts.current.len());
            info!(
                "Delinquent validator count: {}",
                vote_accounts.delinquent.len()
            );

            let mut failures = vec![];

            let total_current_stake = vote_accounts
                .current
                .iter()
                .map(|vote_account| vote_account.activated_stake)
                .sum();
            let total_delinquent_stake = vote_accounts
                .delinquent
                .iter()
                .map(|vote_account| vote_account.activated_stake)
                .sum();

            let total_stake = total_current_stake + total_delinquent_stake;
            let current_stake_percent = total_current_stake as f64 * 100. / total_stake as f64;
            info!(
                "Current stake: {:.2}% | Total stake: {}, current stake: {}, delinquent: {}",
                current_stake_percent,
                Sol(total_stake),
                Sol(total_current_stake),
                Sol(total_delinquent_stake)
            );

            if transaction_count > endpoint.last_transaction_count {
                endpoint.last_transaction_count = transaction_count;
            } else {
                failures.push((
                    "transaction-count",
                    format!(
                        "Transaction count is not advancing: {transaction_count} <= {0}",
                        endpoint.last_transaction_count
                    ),
                ));
            }

            if recent_blockhash != endpoint.last_recent_blockhash {
                endpoint.last_recent_blockhash = recent_blockhash;
            } else {
                failures.push((
                    "recent-blockhash",
                    format!("Unable to get new blockhash: {recent_blockhash}"),
                ));
            }

            if config.monitor_active_stake
                && current_stake_percent < config.active_stake_alert_threshold as f64
            {
                failures.push((
                    "current-stake",
                    format!("Current stake is {current_stake_percent:.2}%"),
                ));
            }

            let mut validator_errors = vec![];
            for validator_identity in config.validator_identity_pubkeys.iter() {
                let formatted_validator_identity =
                    format_labeled_address(&validator_identity.to_string(), &config.address_labels);
                if vote_accounts
                    .delinquent
                    .iter()
                    .any(|vai| vai.node_pubkey == *validator_identity.to_string())
                {
                    validator_errors.push(format!("{formatted_validator_identity} delinquent"));
                } else if !vote_accounts
                    .current
                    .iter()
                    .any(|vai| vai.node_pubkey == *validator_identity.to_string())
                {
                    validator_errors.push(format!("{formatted_validator_identity} missing"));
                }

                if let Some(balance) = validator_balances.get(validator_identity) {
                    if *balance < config.minimum_validator_identity_balance {
                        failures.push((
                            "balance",
                            format!("{} has {}", formatted_validator_identity, Sol(*balance)),
                        ));
                    }
                }
            }

            if !validator_errors.is_empty() {
                failures.push(("delinquent", validator_errors.join(",")));
            }

            for failure in &failures {
                error!("{} sanity failure: {}", failure.0, failure.1);
            }

            Ok(failures.into_iter().next()) // Only report the first failure if any
        }
        Err(err) => {
            if let client_error::ErrorKind::Reqwest(reqwest_err) = err.kind() {
                if let Some(client_error::reqwest::StatusCode::BAD_GATEWAY) = reqwest_err.status() {
                    if config.ignore_http_bad_gateway {
                        warn!("Error suppressed: {err}");
                        return Ok(None);
                    }
                }
            }
            warn!("rpc-error: {err}");
            Err(err)
        }
    }
}

fn validate_endpoints(
    config: &Config,
    endpoints: &Vec<EndpointData>,
) -> Result<(), Box<dyn error::Error>> {
    info!("Validating endpoints...");

    let mut max_slot = 0;
    let mut min_slot = u64::MAX;

    let mut opt_common_genesis_hash: Option<Hash> = None;

    for endpoint in endpoints {
        info!("Querying {}", endpoint.rpc_client.url());

        let slot = endpoint.rpc_client.get_slot()?;
        let genesis_hash = endpoint.rpc_client.get_genesis_hash()?;

        info!("Genesis hash: {genesis_hash}");
        info!("Current slot: {slot}");

        max_slot = max_slot.max(slot);
        min_slot = min_slot.min(slot);

        if let Some(common_genesis_hash) = opt_common_genesis_hash {
            if common_genesis_hash != genesis_hash {
                return Err(
                    "Endpoints don't agree on genesis hash, have you mixed up clusters?".into(),
                );
            }
        } else {
            opt_common_genesis_hash = Some(genesis_hash);
        }
    }

    if max_slot - min_slot > config.acceptable_slot_range {
        return Err(format!(
            "Endpoints slots are too far apart: Acceptable slot range: {}",
            config.acceptable_slot_range,
        )
        .into());
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn error::Error>> {
    agave_logger::setup_with_default_filter();
    solana_metrics::set_panic_hook("watchtower", /*version:*/ None);

    let config = get_config();

    let mut endpoints: Vec<_> = config
        .json_rpc_urls
        .iter()
        .map(|url| EndpointData {
            rpc_client: RpcClient::new_with_timeout(url, config.rpc_timeout),
            last_transaction_count: 0,
            last_recent_blockhash: Hash::default(),
        })
        .collect();

    if let Err(err) = validate_endpoints(&config, &endpoints) {
        error!("Endpoint validation failed: {err}");
        std::process::exit(1);
    }

    let min_agreeing_endpoints = endpoints.len() / 2 + 1;

    let notifier = Notifier::default();

    let mut last_notification_msg = "".into();
    let mut num_consecutive_failures = 0;
    let mut last_success = Instant::now();
    let mut incident = Hash::new_unique();

    loop {
        let mut failures = HashMap::new(); // test_name -> message

        let mut num_healthy = 0;
        let mut num_reachable = 0;

        for endpoint in &mut endpoints {
            match query_endpoint(&config, endpoint) {
                Ok(None) => {
                    num_healthy += 1;
                    num_reachable += 1;
                }
                Ok(Some((failure_test_name, failure_error_message))) => {
                    num_reachable += 1;

                    // Collecting only one failure of each type
                    failures
                        .entry(failure_test_name)
                        .or_insert(failure_error_message.clone());
                }
                Err(_) => {}
            }
        }

        if num_reachable < min_agreeing_endpoints {
            failures.clear(); // Ignoring other failures when watchtower is unreliable

            let watchtower_unreliable_msg = format!(
                "Watchtower is unreliable, {} of {} RPC endpoints are reachable",
                num_reachable,
                endpoints.len()
            );
            failures.insert("watchtower-reliability", watchtower_unreliable_msg);
        }

        if num_healthy < min_agreeing_endpoints {
            if failures.len() > 1 {
                failures.clear(); // Ignoring other failures when watchtower is unreliable

                let watchtower_unreliable_msg = "Watchtower is unreliable, RPC endpoints provide \
                                                 inconsistent information"
                    .into();
                failures.insert("watchtower-reliability", watchtower_unreliable_msg);
            }

            let (failure_test_name, failure_error_message) = failures.iter().next().unwrap();
            let notification_msg = format!(
                "agave-watchtower{}: Error: {}: {}",
                config.name_suffix, failure_test_name, failure_error_message
            );
            num_consecutive_failures += 1;
            if num_consecutive_failures > config.unhealthy_threshold {
                datapoint_info!("watchtower-sanity", ("ok", false, bool));
                if last_notification_msg != notification_msg {
                    notifier.send(&notification_msg, &NotificationType::Trigger { incident });
                }
                datapoint_error!(
                    "watchtower-sanity-failure",
                    ("test", failure_test_name, String),
                    ("err", failure_error_message, String)
                );
                last_notification_msg = notification_msg;
            } else {
                info!(
                    "Failure {} of {}: {}",
                    num_consecutive_failures, config.unhealthy_threshold, notification_msg
                );
            }
        } else {
            datapoint_info!("watchtower-sanity", ("ok", true, bool));
            if !last_notification_msg.is_empty() {
                let alarm_duration = Instant::now().duration_since(last_success);
                let alarm_duration = alarm_duration - config.interval; // Subtract the period before the first error
                let alarm_duration = Duration::from_secs(alarm_duration.as_secs()); // Drop milliseconds in message

                let all_clear_msg = format!(
                    "All clear after {}",
                    humantime::format_duration(alarm_duration)
                );
                info!("{all_clear_msg}");
                notifier.send(
                    &format!("agave-watchtower{}: {}", config.name_suffix, all_clear_msg),
                    &NotificationType::Resolve { incident },
                );
            }
            last_notification_msg = "".into();
            last_success = Instant::now();
            num_consecutive_failures = 0;
            incident = Hash::new_unique();
        }
        sleep(config.interval);
    }
}
