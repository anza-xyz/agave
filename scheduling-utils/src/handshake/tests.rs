use {
    crate::handshake::{
        AgaveHandshakeError, ClientHandshakeError, ClientLogon, client::connect, server::Server,
        shared::MAX_WORKERS,
    },
    agave_scheduler_bindings::{
        PackToWorkerMessage, ProgressMessage, SharableTransactionBatchRegion,
        SharableTransactionRegion, TpuToPackMessage, TransactionResponseRegion,
        WorkerToPackMessage,
    },
    std::time::Duration,
    tempfile::NamedTempFile,
};

#[test]
fn message_passing_on_all_queues() {
    let ipc = NamedTempFile::new().unwrap();
    std::fs::remove_file(ipc.path()).unwrap();
    let mut server = Server::new(ipc.path()).unwrap();

    // Test messages.
    let tpu_to_pack = TpuToPackMessage {
        transaction: SharableTransactionRegion {
            offset: 10,
            length: 5,
        },
        flags: 21,
        src_addr: [4; 16],
    };
    let progress_tracker = ProgressMessage {
        leader_state: agave_scheduler_bindings::LEADER_READY,
        current_slot_progress: 32,
        epoch: 7,
        current_slot: 3,
        next_leader_slot: 12,
        leader_range_end: 16,
        remaining_cost_units: 12_000_000,
        latest_blockhash: [42; 32],
    };
    let pack_to_worker = PackToWorkerMessage {
        flags: 123,
        max_working_slot: 100,
        batch: SharableTransactionBatchRegion {
            num_transactions: 5,
            transactions_offset: 100,
        },
    };
    let worker_to_pack = WorkerToPackMessage {
        batch: SharableTransactionBatchRegion {
            num_transactions: 5,
            transactions_offset: 100,
        },
        processed_code: agave_scheduler_bindings::processed_codes::PROCESSED,
        responses: TransactionResponseRegion {
            tag: 3,
            num_transaction_responses: 2,
            transaction_responses_offset: 1,
        },
    };

    let server_handle = std::thread::spawn(move || {
        let mut session = server.accept().unwrap();

        // Send a tpu_to_pack message.
        session.tpu_to_pack.producer.try_write(tpu_to_pack).unwrap();

        // Send a progress_tracker message.
        session
            .progress_tracker
            .try_write(progress_tracker)
            .unwrap();

        // Receive a pack_to_check_worker message.
        let msg = loop {
            if let Some(msg) = session.pack_to_check_worker.try_read() {
                break msg;
            }
        };
        assert_eq!(msg, pack_to_worker);

        // Send a check_worker_to_pack message.
        session
            .check_worker_to_pack
            .try_write(worker_to_pack)
            .unwrap();

        // Receive pack_to_worker messages.
        for (i, worker) in session.workers.iter_mut().enumerate() {
            let msg = loop {
                if let Some(msg) = worker.pack_to_worker.try_read() {
                    break msg;
                }
            };
            assert_eq!(
                PackToWorkerMessage {
                    max_working_slot: pack_to_worker.max_working_slot + i as u64,
                    ..pack_to_worker
                },
                msg
            );
        }

        // Send worker_to_pack messages.
        for (i, worker) in session.workers.iter_mut().enumerate() {
            worker
                .worker_to_pack
                .try_write(WorkerToPackMessage {
                    batch: SharableTransactionBatchRegion {
                        num_transactions: worker_to_pack.batch.num_transactions + i as u8,
                        ..worker_to_pack.batch
                    },
                    ..worker_to_pack
                })
                .unwrap();
        }
    });
    let client_handle = std::thread::spawn(move || {
        let mut session = connect(
            ipc,
            ClientLogon {
                worker_count: 4,
                allocator_size: 1024 * 1024 * 1024,
                allocator_handles: 3,
                tpu_to_pack_capacity: 65536,
                progress_tracker_capacity: 256,
                pack_to_worker_capacity: 1024,
                worker_to_pack_capacity: 1024,
                flags: 0,
                pack_to_check_worker_capacity: 1024,
                check_worker_to_pack_capacity: 1024,
            },
            Duration::from_secs(1),
        )
        .unwrap();

        // Receive tpu_to_pack message.
        let msg = loop {
            if let Some(msg) = session.tpu_to_pack.try_read() {
                break msg;
            };
        };
        assert_eq!(msg, tpu_to_pack);

        // Receive progress_tracker message.
        let msg = loop {
            if let Some(msg) = session.progress_tracker.try_read() {
                break msg;
            };
        };
        assert_eq!(msg, progress_tracker);

        // Send a pack_to_check_worker message.
        session
            .pack_to_check_worker
            .try_write(pack_to_worker)
            .unwrap();

        // Receive a check_worker_to_pack message.
        let msg = loop {
            if let Some(msg) = session.check_worker_to_pack.try_read() {
                break msg;
            }
        };
        assert_eq!(msg, worker_to_pack);

        // Send pack_to_worker messages.
        for (i, worker) in session.workers.iter_mut().enumerate() {
            worker
                .pack_to_worker
                .try_write(PackToWorkerMessage {
                    max_working_slot: pack_to_worker.max_working_slot + i as u64,
                    ..pack_to_worker
                })
                .unwrap();
        }

        // Receive worker_to_pack messages.
        for (i, worker) in session.workers.iter_mut().enumerate() {
            let msg = loop {
                if let Some(msg) = worker.worker_to_pack.try_read() {
                    break msg;
                }
            };
            assert_eq!(
                WorkerToPackMessage {
                    batch: SharableTransactionBatchRegion {
                        num_transactions: worker_to_pack.batch.num_transactions + i as u8,
                        ..worker_to_pack.batch
                    },
                    ..worker_to_pack
                },
                msg
            );
        }
    });

    client_handle.join().unwrap();
    server_handle.join().unwrap();
}

#[test]
fn check_worker_queues_use_dedicated_capacities() {
    const CHECK_REQUEST_CAPACITY: usize = 1 << 18;
    const CHECK_RESPONSE_CAPACITY: usize = 1 << 19;

    let logon = ClientLogon {
        worker_count: 1,
        allocator_size: 64 * 1024 * 1024,
        allocator_handles: 1,
        tpu_to_pack_capacity: 2,
        progress_tracker_capacity: 2,
        pack_to_worker_capacity: 2,
        worker_to_pack_capacity: 2,
        flags: 0,
        pack_to_check_worker_capacity: CHECK_REQUEST_CAPACITY,
        check_worker_to_pack_capacity: CHECK_RESPONSE_CAPACITY,
    };
    let (_agave, files) = Server::setup_session(logon).unwrap();

    assert!(
        files[3].metadata().unwrap().len()
            >= u64::try_from(shaq::mpmc::minimum_file_size::<PackToWorkerMessage>(
                CHECK_REQUEST_CAPACITY
            ))
            .unwrap()
    );
    assert!(
        files[4].metadata().unwrap().len()
            >= u64::try_from(shaq::mpmc::minimum_file_size::<WorkerToPackMessage>(
                CHECK_RESPONSE_CAPACITY
            ))
            .unwrap()
    );

    crate::handshake::client::setup_session(&logon, files).unwrap();
}

#[test]
fn accept_worker_count_max() {
    let ipc = NamedTempFile::new().unwrap();
    std::fs::remove_file(ipc.path()).unwrap();
    let mut server = Server::new(ipc.path()).unwrap();

    let server_handle = std::thread::spawn(move || {
        let res = server.accept();
        assert!(res.is_ok());
    });
    let client_handle = std::thread::spawn(move || {
        let res = connect(
            ipc,
            ClientLogon {
                worker_count: MAX_WORKERS,
                allocator_size: 1024 * 1024 * 1024,
                allocator_handles: 3,
                tpu_to_pack_capacity: 65536,
                progress_tracker_capacity: 256,
                pack_to_worker_capacity: 1024,
                worker_to_pack_capacity: 1024,
                flags: 0,
                pack_to_check_worker_capacity: 1024,
                check_worker_to_pack_capacity: 1024,
            },
            Duration::from_secs(1),
        );
        assert!(res.is_ok());
    });

    client_handle.join().unwrap();
    server_handle.join().unwrap();
}

#[test]
fn reject_worker_count_low() {
    let ipc = NamedTempFile::new().unwrap();
    std::fs::remove_file(ipc.path()).unwrap();
    let mut server = Server::new(ipc.path()).unwrap();

    let server_handle = std::thread::spawn(move || {
        let res = server.accept();
        let Err(AgaveHandshakeError::WorkerCount(count)) = res else {
            panic!();
        };
        assert_eq!(count, 0);
    });
    let client_handle = std::thread::spawn(move || {
        let res = connect(
            ipc,
            ClientLogon {
                worker_count: 0,
                allocator_size: 1024 * 1024 * 1024,
                allocator_handles: 3,
                tpu_to_pack_capacity: 65536,
                progress_tracker_capacity: 256,
                pack_to_worker_capacity: 1024,
                worker_to_pack_capacity: 1024,
                flags: 0,
                pack_to_check_worker_capacity: 1024,
                check_worker_to_pack_capacity: 1024,
            },
            Duration::from_secs(1),
        );
        let Err(ClientHandshakeError::Rejected(reason)) = res else {
            panic!();
        };
        assert_eq!(reason, "Worker count; count=0");
    });

    client_handle.join().unwrap();
    server_handle.join().unwrap();
}

#[test]
fn reject_worker_count_high() {
    let ipc = NamedTempFile::new().unwrap();
    std::fs::remove_file(ipc.path()).unwrap();
    let mut server = Server::new(ipc.path()).unwrap();

    let server_handle = std::thread::spawn(move || {
        let res = server.accept();
        let Err(AgaveHandshakeError::WorkerCount(count)) = res else {
            panic!();
        };
        assert_eq!(count, 100);
    });
    let client_handle = std::thread::spawn(move || {
        let res = connect(
            ipc,
            ClientLogon {
                worker_count: 100,
                allocator_size: 1024 * 1024 * 1024,
                allocator_handles: 3,
                tpu_to_pack_capacity: 65536,
                progress_tracker_capacity: 256,
                pack_to_worker_capacity: 1024,
                worker_to_pack_capacity: 1024,
                flags: 0,
                pack_to_check_worker_capacity: 1024,
                check_worker_to_pack_capacity: 1024,
            },
            Duration::from_secs(1),
        );
        let Err(ClientHandshakeError::Rejected(reason)) = res else {
            panic!();
        };
        assert_eq!(reason, "Worker count; count=100");
    });

    client_handle.join().unwrap();
    server_handle.join().unwrap();
}
