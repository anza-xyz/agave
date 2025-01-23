use {
    agave_thread_manager::*,
    log::info,
    std::{io::Read, path::PathBuf, time::Duration},
    tokio::sync::oneshot,
};

mod common;
use common::*;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let experiments = [
        "examples/core_contention_dedicated_set.toml",
        "examples/core_contention_contending_set.toml",
    ];

    for exp in experiments {
        info!("===================");
        info!("Running {exp}");
        let mut conf_file = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        conf_file.push(exp);
        let mut buf = String::new();
        std::fs::File::open(conf_file)?.read_to_string(&mut buf)?;
        let cfg: ThreadManagerConfig = toml::from_str(&buf)?;

        let manager = ThreadManager::new(&cfg).unwrap();
        let tokio1 = manager.get_tokio("axum1");
        tokio1.start_metrics_sampling(Duration::from_secs(1));
        let tokio2 = manager.get_tokio("axum2");
        tokio2.start_metrics_sampling(Duration::from_secs(1));

        let workload_runtime = TokioRuntime::new(
            "LoadGenerator".to_owned(),
            TokioConfig {
                core_allocation: CoreAllocation::DedicatedCoreSet { min: 32, max: 64 },
                ..Default::default()
            },
        )?;

        let results = std::thread::scope(|scope| {
            let (tx1, rx1) = oneshot::channel();
            let (tx2, rx2) = oneshot::channel();

            scope.spawn(|| {
                tokio1.tokio.block_on(axum_main(8888, tx1));
            });
            scope.spawn(|| {
                tokio2.tokio.block_on(axum_main(8889, tx2));
            });

            // Wait for axum servers to start
            rx1.blocking_recv().unwrap();
            rx2.blocking_recv().unwrap();

            let join_handle =
                scope.spawn(|| workload_runtime.block_on(workload_main(&[8888, 8889], 1000)));
            join_handle.join().expect("Load generator crashed!")
        });
        //print out the results of the bench run
        info!("Results are: {:?}", results);
    }
    Ok(())
}
