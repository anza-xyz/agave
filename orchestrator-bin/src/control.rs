use {
    crate::{
        args::Args,
        component::{Component, Role},
    },
    agave_orchestrator::{Config, scheduler},
    command_fds::{CommandFdExt, FdMapping},
    futures::{StreamExt, future::OptionFuture, stream::FuturesUnordered},
    std::{
        io::Read,
        os::{fd::AsFd, unix::net::UnixStream},
        path::Path,
        process::Stdio,
        time::Duration,
    },
    tokio::{
        io::AsyncReadExt,
        net::UnixStream as TokioUnixStream,
        process::{Child as TokioChild, Command},
        signal::unix::SignalKind,
    },
};

pub(crate) struct ControlThread {
    args: Args,
    config: Config,

    validator_rx: TokioUnixStream,
    components: FuturesUnordered<Component>,
}

impl ControlThread {
    pub(crate) fn run_in_place(args: Args, config: Config) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(ControlThread::setup(args, config));

        rt.block_on(server.run())
    }

    async fn setup(args: Args, config: Config) -> Self {
        // SAFETY:
        // - FD 3 was mapped by the parent process via command-fds.
        // - `orchestrator_uds` validates it is an open socket.
        let mut validator_rx = unsafe { agave_orchestrator::orchestrator_uds() };

        // Set CLOEXEC so the scheduler child does not inherit this FD.
        nix::fcntl::fcntl(
            &validator_rx,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )
        .expect("set CLOEXEC on validator stream");

        // Wait for agave readiness (banking is ready to start accepting shmem).
        let mut buf = [0u8; 1];
        validator_rx
            .read_exact(&mut buf)
            .expect("read readiness byte");
        assert_eq!(buf[0], 0x01, "unexpected readiness byte");
        log::info!("Validator is ready");

        // Wrap validator stream.
        validator_rx.set_nonblocking(true).unwrap();
        let validator_rx = TokioUnixStream::from_std(validator_rx).unwrap();

        // Setup thread structure.
        let mut thread = ControlThread {
            args,
            config,

            validator_rx,
            components: FuturesUnordered::new(),
        };

        // Spawn initial topology.
        thread.spawn_components().unwrap();

        thread
    }

    async fn run(mut self) {
        let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate()).unwrap();
        let mut sigint = tokio::signal::unix::signal(SignalKind::interrupt()).unwrap();
        let mut sighup = tokio::signal::unix::signal(SignalKind::hangup()).unwrap();

        loop {
            tokio::select! {
                _ = sigterm.recv() => {
                    log::info!("SIGTERM caught, stopping");

                    break;
                },
                _ = sigint.recv() => {
                    log::info!("SIGINT caught, stopping");

                    break;
                },
                _ = sighup.recv() => {
                    log::info!("SIGHUP caught, reloading config");
                    if let Err(err) = self.load_config(&self.args.config.clone()).await {
                        log::error!("Failed to reload config; err={err}");

                        continue;
                    }

                    if let Err(err) = self.cycle().await {
                        log::error!("Failed to cycle components; err={err}");

                        self.fallback().await;
                    }
                },

                () = Self::read_until_eof(&mut self.validator_rx) => {
                    log::error!("Validator exited unexpectedly");

                    break;
                },
                opt = self.components.next() => {
                    let (role, status) = opt.unwrap();
                    log::error!("Component exited unexpectedly; role={role:?}; status={status}");

                    // Fallback until we're okay or we run out of fallbacks.
                    self.fallback().await;
                },
            };
        }

        self.shutdown_components().await;

        log::info!("Exiting");
    }

    async fn fallback(&mut self) {
        loop {
            // Load the next generation.
            let Some(fallback) = self.config.fallback.clone() else {
                panic!("No additional fallbacks configured");
            };
            self.load_config(&fallback)
                .await
                .expect("fallback config failed to load");

            // Try to fallback.
            log::info!("Falling back; fallback={}", fallback.display());
            if let Err(err) = self.cycle().await {
                log::warn!("Fallback failed to cycle; err={err}");

                continue;
            }

            break;
        }
    }

    async fn load_config(&mut self, config: &Path) -> anyhow::Result<()> {
        self.config = toml::from_slice(&std::fs::read(config)?)?;

        Ok(())
    }

    async fn cycle(&mut self) -> anyhow::Result<()> {
        // Tear down existing components.
        self.shutdown_components().await;

        // Spawn new components.
        self.spawn_components()?;

        Ok(())
    }

    async fn shutdown_components(&mut self) {
        // Signal shutdown to remaining components.
        for component in self.components.iter_mut() {
            component.shutdown();
        }

        // Wait for remaining components to exit.
        let mut timeout = Some(Box::pin(tokio::time::sleep(Duration::from_secs(5))));
        loop {
            tokio::select! {
                biased;

                opt = self.components.next() => match opt {
                    Some((role, status)) => log::info!("Component exited; role={role:?}; status={status}"),
                    None => break,
                },
                Some(_) = OptionFuture::from(timeout.as_mut()) => {
                    timeout = None;
                    log::warn!("Timed out waiting for shutdown, killing remaining");
                    for component in self.components.iter_mut() {
                        component.kill();
                    }
                }
            }
        }
    }

    fn spawn_components(&mut self) -> anyhow::Result<()> {
        assert!(self.components.is_empty());

        // Grab scheduler topology.
        let header = scheduler::SessionHeader::new(
            self.config.topology.scheduler.worker_count,
            self.config.topology.scheduler.allocator_handles,
            self.config.topology.scheduler.flags,
        );

        // Allocate all shared memory regions.
        let files = agave_orchestrator::scheduler::create_session(&self.config.topology.scheduler);
        log::info!("Created shmem; fds={}", files.len());

        // Send shmem FDs to agave.
        agave_orchestrator::scheduler::send_session(self.validator_rx.as_fd(), &files, header);
        log::info!("Sent session to validator");

        // Create a fresh UDS pair for orchestrator <> scheduler communication.
        let (scheduler_rx, scheduler_tx) =
            UnixStream::pair().expect("create orchestrator <> scheduler UDS pair");

        // Send shmem FDs to scheduler.
        agave_orchestrator::scheduler::send_session(scheduler_rx.as_fd(), &files, header);
        log::info!("Sent session to scheduler");

        // Spawn the external scheduler, passing the scheduler end at the well-known fd.
        let scheduler = Self::spawn_scheduler(&self.config, scheduler_tx)?;
        log::info!("Spawned scheduler; pid={}", scheduler.id().unwrap());

        // Convert both streams to async for monitoring.
        scheduler_rx.set_nonblocking(true).unwrap();
        let scheduler_rx = TokioUnixStream::from_std(scheduler_rx).unwrap();

        // Store all components for monitoring.
        self.components.extend([Component::new(
            Role::BlockProductionScheduler,
            scheduler,
            scheduler_rx,
        )]);

        Ok(())
    }

    fn spawn_scheduler(config: &Config, scheduler_tx: UnixStream) -> anyhow::Result<TokioChild> {
        let mut cmd = std::process::Command::new(&config.scheduler.bin);
        if let Some(cfg) = &config.scheduler.config {
            cmd.args(["--config", &cfg.to_string_lossy()]);
        }

        Self::spawn_component(cmd, scheduler_tx, config.scheduler.affinity.as_deref())
    }

    fn spawn_component(
        mut cmd: std::process::Command,
        child_uds: UnixStream,
        affinity: Option<&[usize]>,
    ) -> anyhow::Result<TokioChild> {
        // Don't inherit stdout/stderr.
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        // Map the child end of the UDS pair to the well-known fd.
        cmd.fd_mappings(vec![FdMapping {
            parent_fd: child_uds.into(),
            child_fd: agave_orchestrator::ORCHESTRATOR_FD,
        }])
        .unwrap();

        // Apply CPU affinity if configured.
        #[cfg(target_os = "linux")]
        if let Some(cores) = affinity {
            validate_affinity(cores)?;

            // Clone into a Vec owned by the closure (no allocations in pre_exec).
            let cores = cores.to_vec();
            unsafe {
                use std::os::unix::process::CommandExt;

                cmd.pre_exec(move || {
                    // Setup our affinity mask.
                    let mut set: libc::cpu_set_t = std::mem::zeroed();
                    libc::CPU_ZERO(&mut set);
                    for &core in &cores {
                        libc::CPU_SET(core, &mut set);
                    }

                    // Set our new process's affinity.
                    if libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }

                    Ok(())
                });
            }
        }
        #[cfg(not(target_os = "linux"))]
        if affinity.is_some() {
            log::warn!("Affinity is Linux only, ignoring");
        }

        Ok(Command::from(cmd).spawn()?)
    }

    async fn read_until_eof(stream: &mut TokioUnixStream) {
        let mut buf = [0u8; 64];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => return,
                Ok(_) => continue,
                Err(err) => {
                    log::error!("UDS read error; err={err}");
                    return;
                }
            }
        }
    }
}

/// Checks that the requested affinity cores are all present in our own affinity mask.
///
/// Runs in the parent process (no signal-safety constraints).
#[cfg(target_os = "linux")]
fn validate_affinity(cores: &[usize]) -> anyhow::Result<()> {
    if cores.is_empty() {
        anyhow::bail!("empty affinity mask");
    }

    // SAFETY:
    // - `cpu_set_t` is valid when zeroed.
    // - `sched_getaffinity` writes into it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::sched_getaffinity(
            0, // current process
            std::mem::size_of_val(&set),
            &mut set,
        )
    };
    if rc != 0 {
        anyhow::bail!(
            "sched_getaffinity failed; err={}",
            std::io::Error::last_os_error()
        );
    }

    for &core in cores {
        // SAFETY: CPU_ISSET is safe for any initialized cpu_set_t.
        let in_set = unsafe { libc::CPU_ISSET(core, &set) };
        if !in_set {
            anyhow::bail!("core not in orchestrator's affinity mask; core={core}");
        }
    }

    Ok(())
}
