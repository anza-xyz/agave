use std::{io::ErrorKind, path::PathBuf};
#[cfg(unix)]
use {
    crate::banking_stage::BankingControlMsg,
    agave_scheduler_handshake::server::Server,
    std::{os::unix::fs::symlink, path::Path},
    tokio::sync::mpsc,
};

/// Keeps the socket paths alive until validator teardown, independently of the accept thread.
pub(crate) struct BindingsGuard {
    ipc_symlink: PathBuf,
    _socket_dir: tempfile::TempDir,
}

impl Drop for BindingsGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.ipc_symlink)
            && err.kind() != ErrorKind::NotFound
        {
            warn!("Failed to remove scheduler bindings symlink: {err}");
        }
    }
}

#[cfg(unix)]
pub(crate) fn spawn(path: &Path, session_sender: mpsc::Sender<BankingControlMsg>) -> BindingsGuard {
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

    BindingsGuard {
        ipc_symlink: path.to_owned(),
        _socket_dir: socket_dir,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn dropping_guard_removes_socket_and_ledger_symlink() {
        let ledger = tempfile::tempdir().unwrap();
        let ipc_symlink = ledger.path().join("scheduler_bindings.ipc");
        let (sender, _receiver) = mpsc::channel(1);
        let guard = spawn(&ipc_symlink, sender);
        let socket_path = std::fs::read_link(&ipc_symlink).unwrap();
        let socket_dir = socket_path.parent().unwrap().to_owned();
        assert!(socket_path.exists());

        drop(guard);

        assert_eq!(
            std::fs::symlink_metadata(&ipc_symlink).unwrap_err().kind(),
            ErrorKind::NotFound,
        );
        assert!(!socket_dir.exists());
        assert!(ledger.path().exists());
    }
}
