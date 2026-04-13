# Agave Orchestrator

## Why do we split into separate processes?

Splitting out these components enables a few advantages:

- Memory & performance isolation. Some components would be better off with
  different memory allocation & threading strategies, however, this becomes
  tricky if all components share the same process & address space.
- We can safely swap out implementations at runtime (most notably change
  schedulers at runtime).
- With hard API boundaries, rewriting components becomes safer - we could have
  an experimental QUIC server without feature flag/enum dispatch hell.

To realize these benefits, we believe `agave-orchestrator` is necessary. Its
role is to manage the lifecycle of these components and broker the shared memory
that connects them.

## How do we split into separate processes?

Agave could be split into separate processes:

- QUIC
- Core
- Scheduler
- Sigverify
- Broadcast
- Similar logically separable components.

These processes will communicate via shared memory. Core (which contains
accounts DB) will be the primary long-lived process (as it is highly stateful &
slow to restart - accounts db needs to reload).

## Orchestrator's two jobs

1. Set up shared memory regions and distribute FDs to components.
2. Spawn and manage child processes.

systemd only replaces job #2. In doing so it makes job #1 harder.

## Why shmem brokering needs a central process

Without a broker, processes would use named shared memory (`/dev/shm/`). This
has problems: stale files after crashes, path length limits, collision risk if
two instances run, and both sides must independently parse and agree on the
topology. A central manager could coordinate the named paths -- but at that point
you already have an orchestrator, so why not skip the filesystem indirection?

The orchestrator creates anonymous shmem via `memfd_create` and passes FDs
directly over UDS (`SCM_RIGHTS`). No filesystem paths, no races, no cleanup.

## Why spawning matters

If the orchestrator spawns the scheduler, it knows the PID and what FDs to hand
it. If systemd spawns the scheduler independently, the broker has to accept
connections from processes that _claim_ to be the scheduler. This means you
inherit new failure modes: two schedulers connect, zero connect, one connects at
the wrong time, operator misconfigures unit ordering.

There is one valid topology. systemd lets operators express arbitrary
configurations - here that's a liability.

## Config Centralization

The orchestrator configuration file contains the full Agave topology. Instead of
N systemd units, you have 1 config file that defines the runtime architecture.
This makes it easier for operators to manage (they can just wrap orchestrator in
systemd and easily start/stop).

## Hot-swapping

If you want to swap out schedulers, simply update the orchestrator config file &
trigger a hot reload. Orchestrator will handle the correct shutdown & spawning
of fresh processes (while keeping core running to avoid accounts DB reload).

## Cleanup

- Orchestrator detects child death via UDS EOF, sends SIGTERM (then SIGKILL).
- In prod, systemd is the last line of defense for the validator service tree.

## TLDR

systemd removes the easy job (spawning) and adds complexity to the hard job
(shmem brokering + identity). The orchestrator does both, and the process
management part is straightforward once you already have the broker.
