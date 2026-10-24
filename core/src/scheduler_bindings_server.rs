use {
    crate::banking_stage::BankingControlMsg,
    agave_scheduler_handshake::{AgaveSession, server::Server},
    std::path::Path,
    tokio::sync::mpsc,
};

pub(crate) fn spawn(path: &Path, session_sender: mpsc::Sender<BankingControlMsg>) {
    spawn_listener(bind(path), session_sender);
}

/// Like [`spawn`], but first blocks until an external scheduler connects and its session has been
/// sent to banking stage. Later sessions replace it, as on a validator.
///
/// Must not be called from within a tokio runtime.
#[cfg(feature = "dev-context-only-utils")]
pub(crate) fn spawn_after_first_session(
    path: &Path,
    session_sender: mpsc::Sender<BankingControlMsg>,
) {
    let mut listener = bind(path);
    let session = accept(&mut listener);
    session_sender
        .blocking_send(BankingControlMsg::External { session })
        .expect("banking control receiver must be alive");
    spawn_listener(listener, session_sender);
}

fn bind(path: &Path) -> Server {
    // NB: Panic on start if we can't bind.
    let _ = std::fs::remove_file(path);
    Server::new(path).expect("scheduler bindings server must bind")
}

fn accept(listener: &mut Server) -> AgaveSession {
    loop {
        match listener.accept() {
            Ok(session) => return session,
            Err(err) => error!("External scheduler handshake failed; err={err}"),
        }
    }
}

fn spawn_listener(mut listener: Server, session_sender: mpsc::Sender<BankingControlMsg>) {
    std::thread::Builder::new()
        .name("solBindingSrv".to_string())
        .spawn(move || {
            loop {
                let session = accept(&mut listener);
                if session_sender
                    .blocking_send(BankingControlMsg::External { session })
                    .is_err()
                {
                    break;
                }
            }
        })
        .unwrap();
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::validator::SCHEDULER_BINDINGS_IPC_FILENAME,
        agave_scheduler_handshake::{ClientHandshakeError, ClientLogon, ClientSession, client},
        std::time::{Duration, Instant},
    };

    fn logon() -> ClientLogon {
        ClientLogon {
            worker_count: 1,
            check_worker_count: 1,
            allocator_size: 64 * 1024 * 1024,
            allocator_handles: 1,
            tpu_to_pack_capacity: 65536,
            progress_tracker_capacity: 256,
            pack_to_worker_capacity: 1024,
            worker_to_pack_capacity: 1024,
            flags: 0,
            pack_to_check_worker_capacity: 1024,
            check_worker_to_pack_capacity: 1024,
        }
    }

    /// Retries until the server has bound `path`, for up to 5 seconds.
    fn connect(path: &Path, logon: ClientLogon) -> Result<ClientSession, ClientHandshakeError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match client::connect(path, logon, Duration::from_secs(1)) {
                Err(ClientHandshakeError::Io(_)) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                result => return result,
            }
        }
    }

    #[test]
    fn test_spawn_after_first_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SCHEDULER_BINDINGS_IPC_FILENAME);
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                // A rejected handshake leaves the server waiting.
                assert!(matches!(
                    connect(&path, ClientLogon::default()),
                    Err(ClientHandshakeError::Rejected(_))
                ));
                connect(&path, logon()).unwrap()
            }
        });

        let (session_sender, mut session_receiver) = mpsc::channel(1);
        // The server waits on its own thread, so a failing client fails the test instead of
        // leaving it blocked.
        let server = std::thread::spawn({
            let path = path.clone();
            move || spawn_after_first_session(&path, session_sender)
        });
        let _first = client.join().unwrap();
        server.join().unwrap();
        assert!(matches!(
            session_receiver.try_recv(),
            Ok(BankingControlMsg::External { .. })
        ));

        // Later sessions are handed over in the background.
        let _second = connect(&path, logon()).unwrap();
        assert!(matches!(
            session_receiver.blocking_recv(),
            Some(BankingControlMsg::External { .. })
        ));
    }
}
