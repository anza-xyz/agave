#![cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
use {
    crate::jemalloc_monitor::*, log::Level, solana_metrics::datapoint::DataPoint,
    std::time::Duration,
};

fn watcher_thread() {
    fn extend_lifetime<'b>(r: &'b str) -> &'static str {
        // SAFETY: it is safe to extend lifetimes here since we can never write any metrics beyond the point
        // where allocator is deinitialized. The function is private so can not be called from outside
        // Metrics can not work with non-static strings due to design limitations.
        unsafe { std::mem::transmute::<&'b str, &'static str>(r) }
    }
    let mut exit = false;
    while !exit {
        view_allocations(|stats| {
            if stats.data.is_empty() {
                exit = true;
            }
            let mut datapoint = DataPoint::new("MemoryBytesAllocatedTotal");
            for (name, counters) in stats.data.iter() {
                let s = counters.view();
                let name = extend_lifetime(std::str::from_utf8(name).unwrap());
                datapoint.add_field_i64(name, s.bytes_allocated_total as i64);
            }
            solana_metrics::submit(datapoint, Level::Info);
            let mut datapoint = DataPoint::new("MemoryBytesDeallocated");
            for (name, counters) in stats.data.iter() {
                let s = counters.view();
                let name = extend_lifetime(std::str::from_utf8(name).unwrap());
                datapoint.add_field_i64(name, s.bytes_deallocated_total as i64);
            }
            solana_metrics::submit(datapoint, Level::Info);
        });
        let (cunnamed, _cproc) = view_global_allocations();
        let mut datapoint = solana_metrics::datapoint::DataPoint::new("MemoryUnnamedThreads");
        datapoint.add_field_i64(
            "bytes_allocated_total",
            cunnamed.bytes_allocated_total as i64,
        );
        datapoint.add_field_i64(
            "bytes_deallocated_total",
            cunnamed.bytes_deallocated_total as i64,
        );
        solana_metrics::submit(datapoint, Level::Info);

        std::thread::sleep(Duration::from_millis(1000));
    }
}

//Agave specific helper to watch for memory usage
pub fn setup_watch_memory_usage() {
    let mut mps = MemPoolStats::default();
    // this list is brittle but there does not appear to be a better way
    // Order of entries matters here, as first matching prefix will be used
    // So solGossip will match solGossipConsume as well
    for thread in [
        "solPohTickProd",
        "solSigVerTpuVote",
        "solRcvrGossip",
        "solSigVerTpu",
        "solClusterInfo",
        "solGossipCons",
        "solGossipWork",
        "solGossip",
        "solRepair",
        "FetchStage",
        "solShredFetch",
        "solReplayTx",
        "solReplayFork",
        "solRayonGlob",
        "solSvrfyShred",
        "solSigVerify",
        "solRetransmit",
        "solRunGossip",
        "solWinInsert",
        "solAccountsLo",
        "solAccounts",
        "solAcctHash",
        "solVoteSigVerTpu",
        "solTrSigVerTpu",
        "solQuicClientRt",
        "solQuicTVo",
        "solQuicTpu",
        "solQuicTpuFwd",
        "solRepairQuic",
        "solTurbineQuic",
    ] {
        mps.add(thread);
    }
    init_allocator(mps);
    std::thread::spawn(watcher_thread);
}
