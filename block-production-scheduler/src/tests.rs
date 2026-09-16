use {
    super::*,
    agave_scheduler_handshake::server::Server,
    std::{io::ErrorKind, path::Path, sync::Arc, thread},
};

fn config(path: &Path) -> Config {
    Config {
        ipc_path: path.to_path_buf(),
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
    }
}

#[test]
fn connection_failure_is_returned() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory.path().join("missing.ipc"));
    let error = run(config, &AtomicBool::new(false)).unwrap_err();
    assert!(
        matches!(error, ClientHandshakeError::Io(error) if error.kind() == ErrorKind::NotFound)
    );
}

#[test]
fn handshake_then_run_until_exit() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory.path().join("scheduler.ipc"));
    let worker_count = config.worker_count;
    let check_worker_count = config.check_worker_count;
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
