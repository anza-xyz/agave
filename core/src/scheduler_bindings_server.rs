use {
    crate::banking_stage::BankingControlMsg,
    agave_scheduler_handshake::server::Server,
    std::{os::unix::fs::symlink, path::Path},
    tokio::sync::mpsc,
};

pub(crate) fn spawn(path: &Path, session_sender: mpsc::Sender<BankingControlMsg>) {
    // NB: Panic on start if we can't bind.
    // Bind at a short path regardless of the ledger path's length.
    let socket_dir = tempfile::Builder::new()
        .prefix("agave-scheduler-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket_path = socket_dir.path().join("scheduler_bindings.ipc");
    let mut listener = Server::new(&socket_path).unwrap();
    let _ = std::fs::remove_file(path);
    symlink(&socket_path, path).unwrap();

    std::thread::Builder::new()
        .name("solBindingSrv".to_string())
        .spawn(move || {
            let _socket_dir = socket_dir;
            loop {
                match listener.accept() {
                    Ok(session) => {
                        if session_sender
                            .blocking_send(BankingControlMsg::External { session })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(err) => {
                        error!("External scheduler handshake failed; err={err}")
                    }
                };
            }
        })
        .unwrap();
}
