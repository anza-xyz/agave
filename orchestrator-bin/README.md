# agave-orchestrator

Opt-in process lifecycle manager for Agave. Spawns and brokers shared memory
for out-of-process components (e.g. an external scheduler).

See [`specs/agave-orchestrator.md`](../specs/agave-orchestrator.md) for design.

## Config

Create an `orchestrator.toml`:

```toml
[topology.scheduler]
worker_count = 4
allocator_size = 67108864
allocator_handles = 8
tpu_to_pack_capacity = 1024
progress_tracker_capacity = 256
pack_to_worker_capacity = 1024
worker_to_pack_capacity = 1024

[orchestrator]
bin = "/path/to/agave-orchestrator"
log = "/path/to/logs/orchestrator.log"

[scheduler]
bin = "/path/to/agave-scheduler"
# affinity = [4, 5, 6, 7]  # optional, Linux only
```

## Run

```bash
agave-validator --orchestrator /path/to/orchestrator.toml
```

## Local dev

```bash
SOLANA_RUN_SH_VALIDATOR_ARGS="--orchestrator /path/to/orchestrator.toml" ./scripts/run.sh
```

## Hot reload

Send `SIGHUP` to the orchestrator process to reload the config and respawn
the scheduler.
