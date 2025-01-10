# thread-manager
Balances machine resources between multiple threaded runtimes.
The purpose is to manage thread contention between different parts
of the code that may benefit from a diverse set of management options.
For example, we may want to have cores 1-4 handling networking via
Tokio, core 5 handling file IO via Tokio, cores 9-16 allocated for
Rayon thread pool, and cores 6-8 available for general use by std::thread.
This will minimize contention for CPU caches and context switches that
would occur if Rayon was entirely unaware it was running side-by-side with
tokio, and each was to spawn as many threads as there are cores.

## Thread pool mapping
Thread manager will, by default, look for a particular named pool, e.g. "solGossip".
Matching is done independently for each type of runtime.
However, if no named pool is found, it will fall back to the "default" thread pool
of the same type (if specified in the config). If the default pool is not specified,
thread pool lookup will fail.

Multiple names can point to the same pool. For example, "solGossipConsume" and
"solSigverify" can both be executed on the same rayon pool named "rayonSigverify".
This, in principle, allows some degree of runtime sharing between different crates
in the codebase without having to manually patch the pointers through.

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
or root privileges to run the resulting process.
To see which policies are supported check (the sources)[./src/policy.rs]
If you use realtime policies, priority to values from 1 (lowest) to 99 (highest) are possible.

## Tokio
Multiple tokio runtimes can be created, and each may be assigned its own pool of CPU cores to run on.
Number of worker and blocking threads is configurable, as are thread priorities for the pool.

## Native
Native threads (std::thread) can be spawned from managed pools, this allows them to inherit a particular
affinity from the pool, as well as to
control the total number of threads made in every pool.

## Rayon
Rayon already manages thread pools well enough, all thread_manager does on top is enforce affinity and
priority for rayon threads. Normally one would only ever have one rayon pool, but for priority allocations
one may want to spawn many rayon pools.

# Limitations

 * Thread pools can only be created at process startup
 * Once thread pool is created, its policy can not be modified at runtime
 * Thread affinity & priority are not supported outside of linux
 * Thread priority generally requires kernel level support and extra capabilities

# TODO:

 * even more tests
 * better thread priority support


# Examples
 * core_contention_basics will demonstrate why core contention is bad, and how thread configs can help
 * core_contention_sweep will sweep across a range of core counts to show how benefits scale with core counts
