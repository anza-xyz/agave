# agave-dashboard

A web dashboard served by `agave-validator` itself.

```bash
agave-validator --dashboard-port 10999
```

Then open <http://127.0.0.1:10999>. There is nothing else to configure and no
second process to run. The SPA is embedded in the validator binary, and the data
comes from handles the validator already holds.

## Exposing it

The dashboard has no authentication and no TLS. It binds to `127.0.0.1` unless
`--dashboard-bind-address` says otherwise, and everything below assumes you have
decided to serve it to somebody else.

```bash
agave-validator \
  --dashboard-port 10999 \
  --dashboard-allowed-host dashboard.example.com
```

`--dashboard-allowed-host` lists the names the dashboard will answer to; repeat
the flag for each one. `localhost` and any IP literal are accepted without it, so
`http://<your-ip>:10999` works out of the box. A **domain** has to be listed.

A request whose `Host` is not on the list gets a **421** and the page does not
load at all. That is the failure to expect when a proxy forwards a name the
validator has not been told about, and the refused name is in the validator log.

Behind Caddy:

```caddy
dashboard.example.com {
	reverse_proxy 127.0.0.1:10999
}
```

Caddy passes the original `Host` through and upgrades websockets on its own, so
the domain in the site block is the name to allow, and nothing else is needed.

### What the server does on its own

- **Checks `Host`** against the list above before serving anything, so a name
  that resolves to your validator but that you did not list cannot reach the
  page. This is what stops DNS rebinding, which loopback binding does not.
- **Checks `Origin`** on the websocket. Websockets are exempt from the
  same-origin policy, so without this any page open in a viewer's browser could
  connect to a dashboard that viewer can reach.
- **Caps connections** at 256 being served at once, and websockets at 64 of
  those. A refused request gets a 503 rather than a dropped socket.
- **Sends a content security policy** that permits no external code, styles,
  fonts or connections. Images are the one exception, because validator icons
  come from URLs operators publish on chain, and those are limited to `https`.
- **Bounds the work a caller can cause**: 8 KB request heads, 4 KB client
  messages, a 10s header timeout and a 15s write timeout.

### What it does not do

- No authentication, no TLS, no rate limiting, no access log. Those belong in
  the proxy.
- Validator icons are fetched by the viewer's browser from third-party hosts, so
  a viewer's IP address reaches whoever publishes those icons. That is inherent
  to displaying them.
- The identity and vote pubkeys, stake, version and skip rate of your validator
  are all on screen. All of it is already public on chain; none of it is secret,
  but it does identify which validator this is.
- The schedule page shows the stake, client version and gossip address of the
  other validators leading the slots on screen. All of that is already public,
  since every node in the cluster holds it, but serving the page publishes it to
  anyone who can reach it.

### What it costs the validator

The expensive sampling, meaning the cluster-wide validator list and the per-slot
account sweep behind validator names, only runs while at least one viewer is
connected. With nobody watching, the collector does slot bookkeeping and little
else.

The one-off read that maps identities to names runs once, when the collector
attaches. It asks the secondary index which accounts the config program owns and
reads only those, so it costs about what any other account load costs, and it is
skipped entirely on a validator whose index does not cover that program. See
[Validator names](#validator-names).

## Architecture

```
core/src/validator.rs ──► DashboardService
                            ├── server thread     (HTTP + websocket on one port)
                            ├── boot thread       (startup progress until the collector attaches)
                            ├── collector thread  (samples state at 5Hz, diffs, publishes)
                            ├── meters thread     (once-a-second readings: TPS, host, network)
                            └── info loader       (validator names, read once at attach)
```

`context.rs` holds the handles the dashboard reads through. The validator binary
starts the service before the snapshot download, so the page is up through the
slowest part of a cold start, and builds the context from the `Validator` once
`Validator::new` returns. `solana-core` itself is untouched beyond exposing
three handles.

`collect.rs` samples five times a second but publishes only what changed, so an
idle validator produces almost no websocket traffic. `proto.rs` defines the
envelope format and the publisher, which retains the latest value of every key
so a client connecting late is caught up in one shot. `server.rs` routes a
connection by peeking at its request head, then either serves an embedded asset
or hands the untouched socket to soketto.

The SPA lives in `frontend/`. Its `dist` directory is checked in so that a plain
`cargo build` needs no Node toolchain.

## Wire protocol

Every message is a JSON envelope:

```json
{ "topic": "summary", "key": "cluster", "value": "testnet" }
```

Topics currently published:

| Topic     | Keys |
|-----------|------|
| `summary` | `version`, `commit_hash`, `cluster`, `shred_version`, `identity_key`, `identity_name`, `identity_icon`, `vote_key`, `startup_time_nanos`, `server_time_nanos`, `uptime_nanos`, `startup_progress`, `root_slot`, `optimistically_confirmed_slot`, `finalized_slot`, `completed_slot`, `estimated_slot`, `block_height`, `next_leader_slot`, `vote_slot`, `vote_distance`, `identity_balance`, `vote_balance`, `vote_commission`, `stake`, `validator_counts`, `versions`, `estimated_slot_duration_nanos`, `observed_slot_duration_nanos`, `program_cache`, `accounts_cache`, `shreds`, `waterfall`, `slot_waterfalls`, `quic`, `verify`, `executed`, `epoch_remaining_nanos`, `produced_blocks`, `skip_rate`, `health`, `estimated_tps`, `tps_history`, `tps_sample`, `network`, `network_history`, `network_sample`, `threads_history`, `threads_sample`, `ingest_paths` |
| `epoch`   | `new` |
| `peers`   | `all` |
| `slot`    | `overview`, `update`, `upcoming` |

A client can also send a request carrying an `id`, and the reply goes back to
that `id` alone: `summary.ping`, `summary.displays` for the whole name table,
`epoch.query` for a held epoch's schedule, and `slot.range` for a run of slots
out of the packed history.

## Building the frontend

`frontend/dist` is committed to the repository, so a normal `cargo build` needs
no Node toolchain and produces a validator with the UI already inside it. You
only need the steps below when you change the UI, or when `dist` is missing:

```bash
cd dashboard/frontend
npm install
npm test               # pure logic: the bar scale, formatting, the store
npm run build          # writes dist/, which build.rs embeds
```

Then build the validator again and commit the regenerated `dist`. `build.rs`
watches the directory, so cargo picks the new bundle up on its own.

Because `dist` is committed, a reviewer is otherwise asked to accept a bundle
they cannot read. This rebuilds it into a scratch directory and compares the two
file by file:

```bash
npm run verify-dist
```

A mismatch usually means the bundle predates a source change, or was built
against different dependency versions. Run `npm run build` and commit the result.

`npm test` covers the logic that can be tested without a browser: the slot bar
scale, the formatters, the windowing the charts use, and the store's handling of
the message stream. Component rendering is not covered, which would mean pulling
in a DOM implementation and a testing library for the sake of it.

If `dist` is missing, the crate still builds. Cargo prints a warning, and the
server serves a page explaining how to produce the bundle rather than returning
a 404.

`npm run dev` serves the UI on Vite's dev server and proxies `/websocket` to a
validator on `127.0.0.1:10999`. This is the fast loop for UI work, since it needs
no Rust rebuild.

## Validator names

The sidebar and the schedule show validator names where it can find them, and
truncated pubkeys where it cannot.

Names live in accounts owned by the config program. Their addresses are not
derived from the identity they describe, because the tool that publishes one
generates a fresh keypair for it, so there is no address to compute from a
validator's pubkey and the only way to find these accounts is to search by
owner.

That search is affordable against the secondary index and ruinous without it.
Unindexed, the same call reads every account on the validator off disk to check
one field, which on a mainnet node means hundreds of gigabytes and does not
finish in any useful time. The dashboard therefore checks for the index and
skips the search when it is absent, which is why an ordinary validator shows
pubkeys.

To show names instead, start the validator with:

    --account-index program-id     --account-index-include-key Config1111111111111111111111111111111111111

The include key matters. On its own, `--account-index program-id` indexes every
program on the chain, which costs an entry per account at startup and on every
write thereafter. Restricted to the config program it costs two hash lookups per
account and stores a few thousand entries, which against the work index
generation already does per account is not measurable.

## Not yet implemented

Two things have no equivalent here, because the data behind them does not exist
in Agave.

A shred timeline. `shred_fetch_stage` distinguishes turbine from repair via
`PacketFlags::REPAIR`, but nothing records per-shred arrival timing. Recording
it on the packet is not open to us: `solana-packet` is a published crate rather
than a workspace member, so `Meta` cannot gain a field. The timing would have to
be kept alongside and written from `modify_packets`, which handles tens of
thousands of shreds a second, so it would want aggregating per slot rather than
per shred.

Per-slot transaction detail. The fee, compute units and status of each
transaction are written by `TransactionStatusService`, which runs only when a
validator serves RPC with transaction history enabled, or a geyser plugin
requires it. A voting validator with no RPC records none of it, and turning the
flag on grows the blockstore substantially, which is not a trade worth making
for a panel. Without it the blockstore holds the entries and nothing more, so
the panel would come to a list of signatures beside counts this dashboard
already publishes.
