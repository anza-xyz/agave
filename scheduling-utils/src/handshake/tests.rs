use {
    crate::handshake::{
        AgaveHandshakeError, ClientHandshakeError, ClientLogon, ProtocolVersions,
        client::{connect, connect_path},
        server::Server,
        shared::MAX_WORKERS,
    },
    agave_scheduler_bindings::{
        CheckResponseRegion, CheckWorkerToPackMessage, ExecutionResponseRegion,
        ExecutionWorkerToPackMessage, PackToCheckWorkerMessage, PackToExecutionWorkerMessage,
        ProgressMessage, SharableTransactionBatchRegion, SharableTransactionRegion,
        TpuToPackMessage,
    },
    std::time::Duration,
    tempfile::NamedTempFile,
};

#[test]
fn reject_version_mismatches() {
    let current = ProtocolVersions::current();
    let mismatched_versions = [
        ProtocolVersions {
            handshake: current.handshake.checked_add(1).unwrap(),
            ..current
        },
        ProtocolVersions {
            scheduler_bindings: current.scheduler_bindings.checked_add(1).unwrap(),
            ..current
        },
        ProtocolVersions {
            shaq: current.shaq.checked_add(1).unwrap(),
            ..current
        },
        ProtocolVersions {
            rts_alloc: current.rts_alloc.checked_add(1).unwrap(),
            ..current
        },
    ];

    for client_versions in mismatched_versions {
        let ipc = NamedTempFile::new().unwrap();
        std::fs::remove_file(ipc.path()).unwrap();
        let mut server = Server::new(ipc.path()).unwrap();
        let server_handle = std::thread::spawn(move || {
            let Err(AgaveHandshakeError::Version { server, client }) = server.accept() else {
                panic!();
            };
            assert_eq!(server, current);
            assert_eq!(client, client_versions);
        });

        let result = connect_path(
            ipc.path(),
            ClientLogon::default(),
            Duration::from_secs(1),
            client_versions,
        );
        let Err(ClientHandshakeError::Rejected(reason)) = result else {
            panic!();
        };
        assert_eq!(
            reason,
            format!("Version; server=({current}); client=({client_versions})")
        );
        server_handle.join().unwrap();
    }
}

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
        remaining_allocated_accounts_data_size: 20_000_000,
        latest_blockhash: [42; 32],
        target_bank_time_ms: 0,
    };
    let batch = SharableTransactionBatchRegion {
        num_transactions: 5,
        transactions_offset: 100,
    };
    let pack_to_check_worker = PackToCheckWorkerMessage { flags: 123, batch };
    let pack_to_worker = PackToExecutionWorkerMessage {
        flags: 1,
        max_working_slot: 100,
        batch,
    };
    let check_worker_to_pack = CheckWorkerToPackMessage {
        batch,
        processed_code: agave_scheduler_bindings::processed_codes::PROCESSED,
        responses: CheckResponseRegion {
            num_transaction_responses: 2,
            transaction_responses_offset: 1,
        },
    };
    let worker_to_pack = ExecutionWorkerToPackMessage {
        batch,
        processed_code: agave_scheduler_bindings::processed_codes::PROCESSED,
        responses: ExecutionResponseRegion {
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

        assert_eq!(session.check_workers.len(), 2);

        // Receive pack_to_check_worker messages.
        let mut check_messages = Vec::new();
        while check_messages.len() < session.check_workers.len() {
            for worker in &session.check_workers {
                if let Some(msg) = worker.pack_to_check_worker.try_read() {
                    check_messages.push(msg);
                }
            }
        }
        assert_eq!(
            check_messages,
            vec![
                pack_to_check_worker,
                PackToCheckWorkerMessage {
                    batch: SharableTransactionBatchRegion {
                        num_transactions: pack_to_check_worker.batch.num_transactions + 1,
                        ..pack_to_check_worker.batch
                    },
                    ..pack_to_check_worker
                }
            ]
        );

        // Send check_worker_to_pack messages.
        for (i, worker) in session.check_workers.iter().enumerate() {
            worker
                .check_worker_to_pack
                .try_write(CheckWorkerToPackMessage {
                    batch: SharableTransactionBatchRegion {
                        num_transactions: check_worker_to_pack.batch.num_transactions + i as u8,
                        ..check_worker_to_pack.batch
                    },
                    ..check_worker_to_pack
                })
                .unwrap();
        }

        // Receive pack_to_worker messages.
        for (i, worker) in session.workers.iter_mut().enumerate() {
            let msg = loop {
                if let Some(msg) = worker.pack_to_worker.try_read() {
                    break msg;
                }
            };
            assert_eq!(
                PackToExecutionWorkerMessage {
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
                .try_write(ExecutionWorkerToPackMessage {
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
                check_worker_count: 2,
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

        // Send pack_to_check_worker messages.
        for i in 0..2 {
            session
                .pack_to_check_worker
                .try_write(PackToCheckWorkerMessage {
                    batch: SharableTransactionBatchRegion {
                        num_transactions: pack_to_check_worker.batch.num_transactions + i,
                        ..pack_to_check_worker.batch
                    },
                    ..pack_to_check_worker
                })
                .unwrap();
        }

        // Receive check_worker_to_pack messages.
        let mut check_messages = Vec::new();
        while check_messages.len() < 2 {
            if let Some(msg) = session.check_worker_to_pack.try_read() {
                check_messages.push(msg);
            }
        }
        assert_eq!(
            check_messages,
            vec![
                check_worker_to_pack,
                CheckWorkerToPackMessage {
                    batch: SharableTransactionBatchRegion {
                        num_transactions: check_worker_to_pack.batch.num_transactions + 1,
                        ..check_worker_to_pack.batch
                    },
                    ..check_worker_to_pack
                }
            ]
        );

        // Send pack_to_worker messages.
        for (i, worker) in session.workers.iter_mut().enumerate() {
            worker
                .pack_to_worker
                .try_write(PackToExecutionWorkerMessage {
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
                ExecutionWorkerToPackMessage {
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
        check_worker_count: 1,
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
            >= u64::try_from(shaq::mpmc::minimum_file_size::<PackToCheckWorkerMessage>(
                CHECK_REQUEST_CAPACITY
            ))
            .unwrap()
    );
    assert!(
        files[4].metadata().unwrap().len()
            >= u64::try_from(shaq::mpmc::minimum_file_size::<CheckWorkerToPackMessage>(
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
                check_worker_count: 1,
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
                check_worker_count: 1,
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
                check_worker_count: 1,
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

#[test]
fn reject_check_worker_count_low() {
    let ipc = NamedTempFile::new().unwrap();
    std::fs::remove_file(ipc.path()).unwrap();
    let mut server = Server::new(ipc.path()).unwrap();

    let server_handle = std::thread::spawn(move || {
        let res = server.accept();
        let Err(AgaveHandshakeError::CheckWorkerCount(count)) = res else {
            panic!();
        };
        assert_eq!(count, 0);
    });
    let client_handle = std::thread::spawn(move || {
        let res = connect(
            ipc,
            ClientLogon {
                worker_count: 1,
                check_worker_count: 0,
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
        assert_eq!(reason, "Check worker count; count=0");
    });

    client_handle.join().unwrap();
    server_handle.join().unwrap();
}

#[test]
fn reject_check_worker_count_high() {
    let ipc = NamedTempFile::new().unwrap();
    std::fs::remove_file(ipc.path()).unwrap();
    let mut server = Server::new(ipc.path()).unwrap();

    let server_handle = std::thread::spawn(move || {
        let res = server.accept();
        let Err(AgaveHandshakeError::CheckWorkerCount(count)) = res else {
            panic!();
        };
        assert_eq!(count, 100);
    });
    let client_handle = std::thread::spawn(move || {
        let res = connect(
            ipc,
            ClientLogon {
                worker_count: 1,
                check_worker_count: 100,
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
        assert_eq!(reason, "Check worker count; count=100");
    });

    client_handle.join().unwrap();
    server_handle.join().unwrap();
}
