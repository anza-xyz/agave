# thread-manager
Balances machine resources between multiple threaded runtimes. The purpose is to manage thread contention
between different parts of the code that may
benefit from a diverse set of management options. For example, we may want to have cores 1-4 handling
networking via Tokio, core 5 handling file IO via Tokio, cores 9-16 hallocated for Rayon thread pool,
and cores 6-8 available for general use by std::thread. This will minimize contention for CPU caches
and context switches that would occur if Rayon was entirely unaware it was running side-by-side with
tokio, and each was to spawn as many threads as there are cores.

# Supported threading models
## Affinity
All threading models allow setting core affinity, but only on linux

For core affinity you can set e.g.
```toml
core_allocation.DedicatedCoreSet = { min = 16, max = 64 }
```
to pin the pool to cores 16-64.

## Scheduling policy and priority
If you want you can set thread scheduling policy and priority. Keep in mind that this will likely require
```bash
 sudo setcap cap_sys_nice+ep
 ```
or root priviledges to run the resulting process.
To see which policies are supported check (the sources)[./src/policy.rs]
If you use realtime policies, priority to values from 1 (lowest) to 99 (highest) are possible.

## Tokio
Multiple tokio runtimes can be created, and each may be assigned its own pool of CPU cores to run on.
Number of worker and blocking threads is configurable, as are thread priorities for the pool.

## Native
Native threads (std::thread) can be spawned from managed pools, this allows them to inheirt a particular
affinity from the pool, as well as to
control the total number of threads made in every pool.

## Rayon
Rayon already manages thread pools well enough, all thread_manager does on top is enforce affinity and
priority for rayon threads. Normally one would only ever have one rayon pool, but for priority allocations
one may want to spawn many rayon pools.

# Limitations

 * Thread pools can only be created at process startup
 * Once thread pool is created, its policy can not be modified at runtime
 * Thread affinity not supported outside of linux

# TODO:

 * support tracing
 * better metrics integration
 * proper error handling everywhere
 * even more tests


# Examples
All examples need wrk for workload generation. Please install it before running.

 * core_contention_basics will demonstrate why core contention is bad, and how thread configs can help
 * core_contention_sweep will sweep across a range of core counts to show how benefits scale with core counts
