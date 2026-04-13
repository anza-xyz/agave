use {
    command_fds::{CommandFdExt, FdMapping},
    log::info,
    std::{os::unix::net::UnixStream, path::Path, process::Command},
};

/// Spawns the orchestrator and returns agave's side of the UDS pair.
///
/// Unexpected orchestrator exit is detected via the UDS (see
/// `solana_core::banking_stage::orchestrator_server`), which will panic and
/// trigger the panic hook for telemetry.
pub fn spawn_orchestrator(bin: &Path, config_path: &Path) -> UnixStream {
    let (validator_fd, orch_fd) = UnixStream::pair().expect("socketpair failed");

    let mut cmd = Command::new(bin);
    cmd.args(["--config", &config_path.to_string_lossy()]);
    cmd.fd_mappings(vec![FdMapping {
        parent_fd: orch_fd.into(),
        child_fd: agave_orchestrator::ORCHESTRATOR_FD,
    }])
    .expect("fd_mappings failed");

    let child = cmd.spawn().unwrap_or_else(|err| {
        panic!(
            "failed to spawn orchestrator; bin={}; err={err}",
            bin.display()
        )
    });

    info!(
        "Spawned orchestrator child process; pid={}; bin={}",
        child.id(),
        bin.display(),
    );

    // NB: We don't wait on child because we have UDS for exit monitoring.
    drop(child);

    validator_fd
}
