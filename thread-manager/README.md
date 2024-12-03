# thread-manager
Balances machine resources between multiple Tokio runtimes

# Supported threading models
## Tokio
Multiple tokio runtimes can be created, and each may be assigned its own pool of CPU cores to run on.
Number of worker and blocking threads is configurable

## Native
Native threads can be spawned from managed pools, this allows them to inheirt a particular affinity from the pool, as well as to
control the total number of threads made in every pool.

## Rayon
Rayon already manages thread pools, all thread_manager does on top is enforce affinity and priority for rayon threads.
