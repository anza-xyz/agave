# SwQoS MAX_STREAMS: Per-Identity Rate Limiting

## Overview

`SwQosMaxStreamsStreamerCounter` stores a `TokenBucket` per connection table
entry (keyed by identity). The bucket rate-limits stream admission during
server congestion. By default it is permissive (never denies). When the
server enters saturation, the permissive bucket is replaced with an enforcing
one that limits streams to the identity's stake-proportional share.

## Normal Operation (not saturated)

- `on_new_stream` always admits.
- If `rate_limiter_active` is true (leftover from previous episode), the
  bucket is replaced with a permissive dummy and the flag is cleared.
- The permissive bucket has ~unlimited tokens and refill rate — effectively
  a no-op.

## Congestion Episode

1. **Detection**: `LoadDebtTracker` signals saturation when global token
   bucket hits zero (debt). Has hysteresis — recovery requires refilling to
   90% of burst capacity.

2. **First saturated `on_new_stream` call**: `rate_limiter_active` is false.
   Atomically swap the `ArcSwap<TokenBucket>` from permissive to a real one:
   - `initial_tokens = 0` — no free burst at episode start
   - `rate = capacity_tps × (stake / total_stake)` — proportional share
   - `max_tokens = rate × 500ms` — burst cap sized for one congestion episode
   - Set `rate_limiter_active = true`

   Uses `AtomicBool` guard: check before store to avoid redundant swaps.
   ArcSwap store is wait-free so no double-checked locking needed.

3. **Subsequent saturated calls**: `rate_limiter_active` is true, skip
   init. Load bucket from ArcSwap, call `consume_tokens(1)`:
   - `Ok` → admit stream
   - `Err` → drop stream, increment `streams_dropped_on_arrival` stat

4. **Token refill**: `TokenBucket` refills lazily on each `consume_tokens`
   call based on elapsed wall-clock time. No background thread. Identity
   gets a steady stream of tokens at its proportional rate.

5. **Unstaked peers**: Dropped unconditionally during saturation (budget = 0).
   Their bucket is never consulted.

## Recovery

When `on_new_stream` observes `!saturated` and `rate_limiter_active` is
true, it atomically swaps the enforcing bucket with a permissive dummy
and clears the flag. Next congestion episode will rebuild with fresh
stake state (total_stake may have changed).

## Why Lazy Init?

- **Fresh stake state**: Rate computed at congestion onset, not connection
  time. Avoids stale `total_stake` from minutes/hours ago.
- **No accumulated tokens**: Bucket starts at 0 at episode start.
  Long-lived connections don't bank tokens during unsaturated periods.

## Key Constants

- `MAX_RATE_LIMITER_BURST = 500ms` — burst cap duration
- `REFERENCE_RTT = 100ms` — used for BDP/MAX_STREAMS scaling (separate concern)

## Hot Path Cost

- 1 `Relaxed` atomic load (`rate_limiter_active`)
- 1 `ArcSwap::load` (wait-free atomic load, no locks)
- 1 `consume_tokens` (atomic load + CAS + occasional refill)
- `ArcSwap::store` only on episode transitions (rare)
