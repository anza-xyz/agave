use {
    crate::{
        ProgressBar, admin_rpc_service, format_name_value, new_spinner_progress_bar,
        println_name_value,
    },
    console::style,
    solana_clock::Slot,
    solana_commitment_config::CommitmentConfig,
    solana_core::validator::ValidatorStartProgress,
    solana_native_token::Sol,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::{client_error, request},
    solana_validator_exit::Exit,
    std::{
        net::SocketAddr,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, SystemTime},
    },
};

pub struct Dashboard {
    progress_bar: ProgressBar,
    ledger_path: PathBuf,
    exit: Arc<AtomicBool>,
}

impl Dashboard {
    pub fn new(
        ledger_path: &Path,
        log_path: Option<&Path>,
        validator_exit: Option<&mut Exit>,
    ) -> Self {
        println_name_value("Ledger location:", &format!("{}", ledger_path.display()));
        if let Some(log_path) = log_path {
            println_name_value("Log:", &format!("{}", log_path.display()));
        }

        let progress_bar = new_spinner_progress_bar();
        progress_bar.set_message("Initializing...");

        let exit = Arc::new(AtomicBool::new(false));
        if let Some(validator_exit) = validator_exit {
            let exit = exit.clone();
            validator_exit.register_exit(Box::new(move || exit.store(true, Ordering::Relaxed)));
        }

        Self {
            exit,
            ledger_path: ledger_path.to_path_buf(),
            progress_bar,
        }
    }

    pub fn run(self, refresh_interval: Duration) {
        let Self {
            exit,
            ledger_path,
            progress_bar,
            ..
        } = self;
        drop(progress_bar);

        let runtime = admin_rpc_service::runtime();
        while !exit.load(Ordering::Relaxed) {
            let progress_bar = new_spinner_progress_bar();
            progress_bar.set_message("Connecting...");

            let Some((rpc_addr, start_time, contact_info, mut vat_status)) = runtime.block_on(
                wait_for_validator_startup(&ledger_path, &exit, progress_bar, refresh_interval),
            ) else {
                continue;
            };

            let rpc_client = RpcClient::new_socket(rpc_addr);
            let mut identity = match rpc_client.get_identity() {
                Ok(identity) => identity,
                Err(err) => {
                    println!("Failed to get validator identity over RPC: {err}");
                    continue;
                }
            };
            println_name_value("Identity:", &identity.to_string());
            if let Some(status) = vat_status.as_ref()
                && status.voting_enabled
            {
                println_name_value("Vote Account:", &status.vote_account.to_string());
            }

            if let Ok(genesis_hash) = rpc_client.get_genesis_hash() {
                println_name_value("Genesis Hash:", &genesis_hash.to_string());
            }

            if let Ok(version) = rpc_client.get_version() {
                println_name_value("Version:", &version.to_string());
            }
            if let Some(admin_rpc_service::AdminRpcContactInfo {
                gossip,
                rpc,
                rpc_pubsub,
                shred_version,
                tpu_quic,
                ..
            }) = contact_info
            {
                println_name_value("Shred Version:", &shred_version.to_string());
                println_name_value("Gossip Address:", &gossip.to_string());
                if let Some(tpu_quic) = tpu_quic {
                    println_name_value("TPU QUIC Address:", &tpu_quic.to_string());
                }
                if rpc.port() != 0 {
                    println_name_value("JSON RPC URL:", &format!("http://{rpc}"));
                }
                if rpc_pubsub.port() != 0 {
                    println_name_value("WebSocket PubSub URL:", &format!("ws://{rpc_pubsub}"));
                }
            }

            let progress_bar = new_spinner_progress_bar();
            let mut snapshot_slot_info = None;
            let mut admin_client = None;
            for i in 0.. {
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                if i % 10 == 0 {
                    snapshot_slot_info = rpc_client.get_highest_snapshot_slot().ok();
                }

                let new_identity = rpc_client.get_identity().unwrap_or(identity);
                if identity != new_identity {
                    identity = new_identity;
                    progress_bar.println(format_name_value("Identity:", &identity.to_string()));
                    if let Some(status) = vat_status.as_ref()
                        && status.voting_enabled
                    {
                        progress_bar.println(format_name_value(
                            "Vote Account:",
                            &status.vote_account.to_string(),
                        ));
                    }
                }

                if i > 0 && i % 30 == 0 {
                    if admin_client.is_none() {
                        admin_client = runtime
                            .block_on(admin_rpc_service::connect(&ledger_path))
                            .ok();
                    }

                    let vat_status_result = admin_client
                        .as_ref()
                        .map(|admin_client| runtime.block_on(admin_client.vat_status()));
                    vat_status = match vat_status_result {
                        Some(Ok(status)) => Some(status),
                        Some(Err(_err)) => {
                            admin_client = None;
                            None
                        }
                        None => None,
                    };
                }

                match get_validator_stats(&rpc_client, &identity) {
                    Ok((
                        processed_slot,
                        confirmed_slot,
                        finalized_slot,
                        transaction_count,
                        identity_balance,
                        health,
                    )) => {
                        let uptime = {
                            let uptime =
                                chrono::Duration::from_std(start_time.elapsed().unwrap()).unwrap();

                            format!(
                                "{:02}:{:02}:{:02} ",
                                uptime.num_hours(),
                                uptime.num_minutes() % 60,
                                uptime.num_seconds() % 60
                            )
                        };

                        let vat_status_formatted = format_vat_status(vat_status.as_ref());

                        progress_bar.set_message(format!(
                            "{}{}| Processed Slot: {} | Confirmed Slot: {} | Finalized Slot: {} | \
                             Full Snapshot Slot: {} | Incremental Snapshot Slot: {} | \
                             Transactions: {} | {}\n{}",
                            uptime,
                            if health == "ok" {
                                "".to_string()
                            } else {
                                format!("| {} ", style(health).bold().red())
                            },
                            processed_slot,
                            confirmed_slot,
                            finalized_slot,
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
                            transaction_count,
                            identity_balance,
                            vat_status_formatted,
                        ));
                        thread::sleep(refresh_interval);
                    }
                    Err(err) => {
                        progress_bar.abandon_with_message(format!("RPC connection failure: {err}"));
                        break;
                    }
                }
            }
        }
    }
}

fn format_vat_status(
    status: Option<&admin_rpc_service::AdminRpcValidatorAdmissionTicketStatus>,
) -> String {
    let Some(status) = status else {
        return "VAT: failed to connect to admin RPC".to_string();
    };

    if !status.vat_active {
        return "VAT: inactive".to_string();
    }

    format!(
        "{}{}",
        format_current_vat_status(status),
        format_effective_epoch_vat_status(status)
    )
}

fn format_current_vat_status(
    status: &admin_rpc_service::AdminRpcValidatorAdmissionTicketStatus,
) -> String {
    if status.in_current_epoch_vat {
        format!(
            "VAT: epoch {} in (stake: {})",
            status.current_epoch,
            Sol(status.current_epoch_vote_account_stake)
        )
    } else {
        format!("VAT: epoch {} out", status.current_epoch)
    }
}

fn format_effective_epoch_vat_status(
    status: &admin_rpc_service::AdminRpcValidatorAdmissionTicketStatus,
) -> String {
    let vat_effective_epoch = status.current_epoch.saturating_add(2);
    if let Some(vat_failure_reason) = &status.next_epoch_vat_failure_reason {
        format!(", epoch {vat_effective_epoch}: {vat_failure_reason}")
    } else {
        format!(", epoch {vat_effective_epoch}: eligible if staked")
    }
}

async fn wait_for_validator_startup(
    ledger_path: &Path,
    exit: &AtomicBool,
    progress_bar: ProgressBar,
    refresh_interval: Duration,
) -> Option<(
    SocketAddr,
    SystemTime,
    Option<admin_rpc_service::AdminRpcContactInfo>,
    Option<admin_rpc_service::AdminRpcValidatorAdmissionTicketStatus>,
)> {
    let mut admin_client = None;
    loop {
        if exit.load(Ordering::Relaxed) {
            return None;
        }

        if admin_client.is_none() {
            admin_client = Some(match admin_rpc_service::connect(ledger_path).await {
                Ok(new_admin_client) => new_admin_client,
                Err(err) => {
                    progress_bar.set_message(format!("Unable to connect to validator: {err}"));
                    thread::sleep(refresh_interval);
                    continue;
                }
            });
        }

        let start_progress = match admin_client.as_ref().unwrap().start_progress().await {
            Ok(start_progress) => start_progress,
            Err(err) => {
                admin_client = None;
                progress_bar.set_message(format!("Failed to get validator start progress: {err}"));
                thread::sleep(refresh_interval);
                continue;
            }
        };

        if start_progress != ValidatorStartProgress::Running {
            progress_bar.set_message(format!("Validator startup: {start_progress:?}..."));
            thread::sleep(refresh_interval);
            continue;
        }

        let admin_client = admin_client.take().unwrap();
        match get_validator_startup_info(admin_client).await {
            Ok(None) => progress_bar.set_message("RPC service not available"),
            Ok(Some(validator_startup_info)) => return Some(validator_startup_info),
            Err(err) => {
                progress_bar.set_message(format!("Failed to get validator info: {err}"));
            }
        }
        thread::sleep(refresh_interval);
    }
}

async fn get_validator_startup_info(
    admin_client: admin_rpc_service::gen_client::Client,
) -> Result<
    Option<(
        SocketAddr,
        SystemTime,
        Option<admin_rpc_service::AdminRpcContactInfo>,
        Option<admin_rpc_service::AdminRpcValidatorAdmissionTicketStatus>,
    )>,
    jsonrpc_core_client::RpcError,
> {
    let Some(rpc_addr) = admin_client.rpc_addr().await? else {
        return Ok(None);
    };
    let start_time = admin_client.start_time().await?;
    let contact_info = Some(admin_client.contact_info().await?);
    let vat_status = admin_client.vat_status().await.ok();
    Ok(Some((rpc_addr, start_time, contact_info, vat_status)))
}

fn get_validator_stats(
    rpc_client: &RpcClient,
    identity: &Pubkey,
) -> client_error::Result<(Slot, Slot, Slot, u64, Sol, String)> {
    let finalized_slot = rpc_client.get_slot_with_commitment(CommitmentConfig::finalized())?;
    let confirmed_slot = rpc_client.get_slot_with_commitment(CommitmentConfig::confirmed())?;
    let processed_slot = rpc_client.get_slot_with_commitment(CommitmentConfig::processed())?;
    let transaction_count =
        rpc_client.get_transaction_count_with_commitment(CommitmentConfig::processed())?;
    let identity_balance = rpc_client
        .get_balance_with_commitment(identity, CommitmentConfig::confirmed())?
        .value;

    let health = match rpc_client.get_health() {
        Ok(()) => "ok".to_string(),
        Err(err) => {
            if let client_error::ErrorKind::RpcError(request::RpcError::RpcResponseError {
                code: _,
                message: _,
                data:
                    request::RpcResponseErrorData::NodeUnhealthy {
                        num_slots_behind: Some(num_slots_behind),
                    },
            }) = err.kind()
            {
                format!("{num_slots_behind} slots behind")
            } else {
                "health unknown".to_string()
            }
        }
    };

    Ok((
        processed_slot,
        confirmed_slot,
        finalized_slot,
        transaction_count,
        Sol(identity_balance),
        health,
    ))
}

#[cfg(all(test, not(target_family = "windows")))]
mod tests {
    use {
        super::*,
        jsonrpc_core::{Error, IoHandler},
        jsonrpc_ipc_server::ServerBuilder,
        solana_gossip::contact_info::ContactInfo,
        solana_keypair::Keypair,
        solana_signer::Signer,
        std::sync::atomic::AtomicUsize,
    };

    #[test]
    fn test_wait_for_validator_startup_retries_legacy_contact_info() {
        let ledger_path = tempfile::tempdir().unwrap();
        let rpc_addr = "127.0.0.1:8899".parse::<SocketAddr>().unwrap();
        let start_time = SystemTime::now();
        let keypair = Keypair::new();
        let mut contact_info = serde_json::to_value(admin_rpc_service::AdminRpcContactInfo::from(
            ContactInfo::new(keypair.pubkey(), 0, 0),
        ))
        .unwrap();
        contact_info
            .as_object_mut()
            .unwrap()
            .remove("tpu_quic")
            .unwrap();
        let contact_info_attempts = Arc::new(AtomicUsize::new(0));

        let mut io = IoHandler::default();
        io.add_sync_method("startProgress", |_| {
            Ok(serde_json::to_value(ValidatorStartProgress::Running).unwrap())
        });
        io.add_sync_method("rpcAddress", move |_| {
            Ok(serde_json::to_value(Some(rpc_addr)).unwrap())
        });
        io.add_sync_method("startTime", move |_| {
            Ok(serde_json::to_value(start_time).unwrap())
        });
        let attempts = contact_info_attempts.clone();
        io.add_sync_method("contactInfo", move |_| {
            if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                Err(Error::invalid_params(
                    "Retry once validator start up is complete",
                ))
            } else {
                Ok(contact_info.clone())
            }
        });
        let server = ServerBuilder::new(io)
            .start(&ledger_path.path().join("admin.rpc").display().to_string())
            .unwrap();
        let exit = AtomicBool::new(false);
        let (_, _, contact_info, _) = admin_rpc_service::runtime()
            .block_on(wait_for_validator_startup(
                ledger_path.path(),
                &exit,
                new_spinner_progress_bar(),
                Duration::from_millis(1),
            ))
            .unwrap();

        assert!(contact_info.unwrap().tpu_quic.is_none());
        assert_eq!(contact_info_attempts.load(Ordering::Relaxed), 2);
        server.close();
    }
}
