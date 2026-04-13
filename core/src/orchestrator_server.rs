use {
    crate::banking_stage::BankingControlMsg, agave_orchestrator::OrchestratorStream,
    std::io::Write, tokio::sync::mpsc,
};

/// Listens on the orchestrator UDS for hot-swap scheduling sessions.
///
/// Consumes new sessions from the orchestrator and forwards them to the
/// banking stage via `BankingControlMsg::External`. If the orchestrator
/// disconnects unexpectedly, `recv_agave_session` will panic, triggering the
/// panic hook and shutting down agave.
pub(crate) fn spawn(
    stream: OrchestratorStream,
    banking_control_sender: mpsc::Sender<BankingControlMsg>,
) {
    std::thread::Builder::new()
        .name("solOrchSrv".to_string())
        .spawn(move || run(stream, banking_control_sender))
        .unwrap();
}

fn run(mut stream: OrchestratorStream, banking_control_sender: mpsc::Sender<BankingControlMsg>) {
    // Signal that we are now ready to receive shmem.
    match stream.write_all(&[0x01]) {
        Ok(()) => log::info!("Sent readiness signal to orchestrator"),
        Err(err) => log::error!("Failed to send readiness to orchestrator: {err}"),
    }

    loop {
        // NB: Blocking read with no timeout - thread parks on recvmsg until
        //     orchestrator sends a new session or the UDS closes.
        let session = agave_orchestrator::scheduler::recv_agave_session(&stream, None);
        log::info!("Received hot-swap scheduling session from orchestrator");

        if banking_control_sender
            .blocking_send(BankingControlMsg::External { session })
            .is_err()
        {
            log::info!("Banking control channel closed, orchestrator server exiting");
            break;
        }
    }
}
