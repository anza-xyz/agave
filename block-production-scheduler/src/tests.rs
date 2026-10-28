use {
    super::*,
    crate::{
        resolved_transaction::ResolvedTransaction,
        tpu_ingress::{
            MAX_PACKETS_PER_CHECK_BATCH, MAX_TPU_PACKETS_PER_ITERATION, TpuTransactionMeta,
        },
    },
    agave_scheduler_bindings::SharableTransactionRegion,
    agave_scheduler_handshake::{AgaveSession, ClientLogon, server::Server, setup_local_session},
    agave_scheduling_utils::{pubkeys_ptr::OwnedPubkeysPtr, transaction_ptr::OwnedTransactionPtr},
    solana_message::{Message, MessageHeader, VersionedMessage},
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    std::{os::unix::net::UnixListener, path::Path, sync::Arc, thread},
};

mod scheduling_flows;

fn config(path: &Path) -> Config {
    Config {
        ipc_path: path.to_path_buf(),
        log_file: None,
        session: SessionConfig {
            handshake_timeout: Duration::from_secs(1),
            worker_count: 2,
            check_worker_count: 3,
            allocator_size: 64 * 1024 * 1024,
            allocator_handles: 1,
            tpu_to_pack_capacity: 16,
            progress_tracker_capacity: 32,
            pack_to_worker_capacity: 64,
            worker_to_pack_capacity: 128,
            pack_to_check_worker_capacity: 256,
            check_worker_to_pack_capacity: 512,
        },
        scheduler: SchedulerConfig {
            transaction_state_capacity: 512,
            execution_margin: Duration::from_millis(10),
            max_cost_units_per_worker: 1_000_000,
            max_cost_units_per_batch: 1_000_000,
            target_entry_bytes_per_batch: 4_622,
        },
    }
}

pub(super) fn setup(check_capacity: usize) -> (Scheduler, AgaveSession) {
    setup_with_workers(check_capacity, 1)
}

pub(super) fn setup_with_workers(
    check_capacity: usize,
    worker_count: usize,
) -> (Scheduler, AgaveSession) {
    let (agave, client) = setup_local_session(ClientLogon {
        worker_count,
        check_worker_count: 1,
        simulate_worker_count: 0,
        allocator_size: 16 * 1024 * 1024,
        allocator_handles: 1,
        tpu_to_pack_capacity: MAX_TPU_PACKETS_PER_ITERATION
            .get()
            .saturating_add(MAX_PACKETS_PER_CHECK_BATCH)
            .saturating_add(1)
            .next_power_of_two(),
        progress_tracker_capacity: 2,
        pack_to_worker_capacity: 2,
        worker_to_pack_capacity: 2,
        pack_to_check_worker_capacity: check_capacity,
        check_worker_to_pack_capacity: check_capacity,
        pack_to_simulate_worker_capacity: 0,
        simulate_worker_to_pack_capacity: 0,
    })
    .unwrap();
    let mut scheduler = Scheduler::new(
        client,
        512,
        Duration::from_millis(10),
        1_000_000,
        1_000_000,
        4_622,
    )
    .unwrap();
    scheduler.state = SchedulerState::LeaderReady {
        slot: 100,
        remaining_cost_units: 0,
        slot_start: Instant::now(),
        slot_duration: Duration::from_millis(400),
    };
    (scheduler, agave)
}

pub(super) fn insert(
    scheduler: &mut Scheduler,
    priority: u64,
    cost: u64,
    accounts: &[u8],
) -> usize {
    let bytes = wincode::serialize(&VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::Legacy(Message {
            header: MessageHeader {
                num_required_signatures: 1,
                ..Default::default()
            },
            account_keys: accounts
                .iter()
                .map(|&key| Pubkey::from([key; 32]))
                .collect(),
            ..Default::default()
        }),
    })
    .unwrap();
    let allocator = &scheduler.allocator;
    let ptr = allocator.allocate(bytes.len() as u32).unwrap();
    // SAFETY: this fresh allocation is large enough and does not overlap the source.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr(), bytes.len()) };
    let region = SharableTransactionRegion {
        // SAFETY: this allocation belongs to this allocator.
        offset: unsafe { allocator.offset(ptr) },
        length: bytes.len() as u32,
    };
    // SAFETY: the initialized allocation is transferred to the guard.
    let transaction = unsafe { OwnedTransactionPtr::from_region(region, allocator) };
    // SAFETY: no pubkey allocation is needed for a legacy transaction.
    let pubkeys = unsafe { OwnedPubkeysPtr::from_region(None, allocator) };
    // SAFETY: the transaction belongs to this allocator.
    let transaction = unsafe {
        ResolvedTransaction::try_new(
            transaction,
            pubkeys,
            allocator,
            &scheduler.reserved_account_keys,
        )
    }
    .unwrap();
    scheduler
        .transactions
        .insert(
            priority,
            TransactionState {
                transaction,
                metadata: TpuTransactionMeta {
                    flags: 0,
                    src_addr: [0; 16],
                },
                cost,
                allocated_accounts_data_size: 1,
                execution_worker: None,
            },
        )
        .ok()
        .unwrap()
        .0
}

#[test]
fn config_uses_shared_defaults_and_overrides() {
    let config = Config::from_toml("ipc_path = '/ledger/scheduler_bindings.ipc'").unwrap();
    assert!(config.log_file.is_none());
    let session = SessionConfig::default();
    let scheduler = SchedulerConfig::default();
    assert_eq!(config.session.worker_count, session.worker_count);
    assert_eq!(config.session.allocator_size, session.allocator_size);
    assert_eq!(
        config.scheduler.max_cost_units_per_worker,
        scheduler.max_cost_units_per_worker
    );
    assert_eq!(config.scheduler.execution_margin, Duration::from_millis(10));

    let config = Config::from_toml(
        r"
ipc_path = '/ledger/scheduler_bindings.ipc'
log_file = '/logs/scheduler.log'
[session]
worker_count = 2
handshake_timeout_ms = 250
[scheduler]
execution_margin_ms = 5
",
    )
    .unwrap();
    assert_eq!(
        config.log_file.as_deref(),
        Some(Path::new("/logs/scheduler.log"))
    );
    assert_eq!(config.session.worker_count, 2);
    assert_eq!(config.session.handshake_timeout, Duration::from_millis(250));
    assert_eq!(config.scheduler.execution_margin, Duration::from_millis(5));
    assert_eq!(config.session.allocator_size, session.allocator_size);
    assert_eq!(
        config.scheduler.max_cost_units_per_worker,
        scheduler.max_cost_units_per_worker
    );
}

#[test]
fn config_requires_path_and_rejects_unknown_fields() {
    assert!(Config::from_toml("").is_err());
    assert!(Config::from_toml("ipc_path = '/ledger/socket'\nunknown = 1").is_err());
    assert!(Config::from_toml("ipc_path = '/ledger/socket'\n[session]\nworker_cout = 2").is_err());
}

#[test]
fn initializes_pacing_once_per_slot() {
    let (mut scheduler, _agave) = setup(2);
    let start = Instant::now();
    scheduler.state = SchedulerState::LeaderReady {
        slot: 100,
        remaining_cost_units: 100,
        slot_start: start,
        slot_duration: Duration::from_millis(400),
    };
    scheduler.handle_slot_change();
    assert_eq!(
        scheduler.pacing_budget(start + Duration::from_millis(195), 0),
        50
    );

    scheduler.state = SchedulerState::LeaderReady {
        slot: 100,
        remaining_cost_units: 90,
        slot_start: Instant::now(),
        slot_duration: Duration::from_millis(400),
    };
    scheduler.handle_slot_change();
    assert_eq!(
        scheduler.pacing_budget(start + Duration::from_millis(195), 10),
        40
    );

    let next_start = Instant::now();
    scheduler.state = SchedulerState::LeaderReady {
        slot: 101,
        remaining_cost_units: 200,
        slot_start: next_start,
        slot_duration: Duration::from_millis(400),
    };
    scheduler.handle_slot_change();
    assert_eq!(scheduler.pacing_budget(next_start, 0), 0);
    assert_eq!(scheduler.scheduling_slot, Some(101));
    assert_eq!(
        scheduler.pacing_budget(next_start + Duration::from_millis(195), 0),
        100
    );
}

#[test]
fn handshake_then_run_until_exit() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory.path().join("scheduler.ipc"));
    let worker_count = config.session.worker_count;
    let check_worker_count = config.session.check_worker_count;
    assert!(try_connect(&config).unwrap().is_none());
    let mut server = Server::new(&config.ipc_path).unwrap();
    let exit = Arc::new(AtomicBool::new(false));
    let scheduler_exit = exit.clone();
    let scheduler_thread = thread::spawn(move || run(config, &scheduler_exit));

    let session = server.accept();
    exit.store(true, Ordering::Relaxed);
    scheduler_thread.join().unwrap().unwrap();

    let session = session.unwrap();
    assert_eq!(session.workers.len(), worker_count);
    assert_eq!(session.check_workers.len(), check_worker_count);
    assert!(session.simulate_workers.is_empty());
}

#[test]
fn stale_socket_is_retryable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scheduler.ipc");
    drop(UnixListener::bind(&path).unwrap());
    let config = config(&path);
    assert!(try_connect(&config).unwrap().is_none());
}
