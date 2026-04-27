use {
    crate::banking_stage::BankingControlMsg,
    agave_scheduling_utils::handshake,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::Protocol},
    std::{
        net::SocketAddr,
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    },
    tokio::sync::mpsc,
};

const TPU_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Original TPU and TPU forwards addresses captured before the first override.
static ORIGINAL_TPU: Mutex<Option<(Arc<ClusterInfo>, SocketAddr, SocketAddr)>> = Mutex::new(None);

/// Revert the override applied by [`apply_tpu_override`]. No-op if none is active.
pub(crate) fn restore_tpu_override() {
    let Some((cluster_info, tpu, tpu_forwards)) = ORIGINAL_TPU.lock().unwrap().take() else {
        return;
    };

    cluster_info
        .set_tpu_quic(tpu)
        .expect("captured from gossip; cannot fail");
    cluster_info
        .set_tpu_forwards_quic(tpu_forwards)
        .expect("captured from gossip; cannot fail");
    info!("Restored original TPU addresses; tpu={tpu}, tpu_forwards={tpu_forwards}");
}

pub(crate) fn spawn(
    path: &Path,
    session_sender: mpsc::Sender<BankingControlMsg>,
    cluster_info: Arc<ClusterInfo>,
) {
    // NB: Panic on start if we can't bind.
    let _ = std::fs::remove_file(path);
    let mut listener = handshake::server::Server::new(path).unwrap();

    std::thread::Builder::new()
        .name("solBindingSrv".to_string())
        .spawn(move || {
            // Block until gossip has published TPU/TPU forwards.
            while cluster_info.my_contact_info().tpu(Protocol::QUIC).is_none()
                || cluster_info
                    .my_contact_info()
                    .tpu_forwards(Protocol::QUIC)
                    .is_none()
            {
                std::thread::sleep(TPU_POLL_INTERVAL);
            }

            loop {
                match listener.accept() {
                    Ok(session) => {
                        if let Some(tpu_override) = session.tpu_override {
                            apply_tpu_override(&cluster_info, tpu_override);
                        }
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

/// Apply `tpu_override`, capturing originals on the first override.
fn apply_tpu_override(cluster_info: &Arc<ClusterInfo>, tpu_override: SocketAddr) {
    let mut original = ORIGINAL_TPU.lock().unwrap();
    if original.is_none() {
        let contact = cluster_info.my_contact_info();
        let tpu = contact
            .tpu(Protocol::QUIC)
            .expect("TPU published before accept");
        let tpu_forwards = contact
            .tpu_forwards(Protocol::QUIC)
            .expect("TPU forwards published before accept");
        *original = Some((cluster_info.clone(), tpu, tpu_forwards));
    }

    cluster_info
        .set_tpu_quic(tpu_override)
        .expect("address validated at handshake");
    cluster_info
        .set_tpu_forwards_quic(tpu_override)
        .expect("address validated at handshake");

    info!("TPU override applied; addr={tpu_override}");
}
