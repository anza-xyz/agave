# XDP Integration Tests

These tests are marked ignored so ordinary workspace test jobs do not run them without privileges. They are run through `cargo xtask xdp-test`.

The tests run directly on the host and require root or equivalent network admin privileges because the harness creates a temporary network namespace, `veth` interfaces, routes, and neighbors. `xtask` builds the test binaries as the current user, then applies `--runner` only when running the compiled test executables:

```bash
cargo xtask xdp-test --runner "sudo -n -E"
```

To run a single test locally, use this form:

```bash
cargo xtask xdp-test --runner "sudo -n -E" --test <test-binary> -- <test-name> --exact --nocapture
```

The default suite currently runs:

- `netlink_snapshot`
- `route_monitor`

## Test Topology

Each portable test runs in a fresh temporary network namespace created with `unshare(CLONE_NEWNET)`. The tests bring `lo` up, create the interfaces needed by that test, and restore the original namespace when the test exits.

The initial topology is one veth pair inside that namespace:

```text
temporary test network namespace

  route and neighbor state under test
        |
        v
  axdp0 10.0.0.1/24  02:aa:bb:cc:dd:01
        |
        | veth peer
        |
  axdp1 10.0.0.2/24  02:aa:bb:cc:dd:02

  neighbor: 10.0.0.2 -> 02:aa:bb:cc:dd:02 dev axdp0
  route example: 203.0.113.0/24 via 10.0.0.2 dev axdp0
```

## Individual Tests

Use the single-test command form above with these test binaries and names:

| Test binary | Test name |
| --- | --- |
| `netlink_snapshot` | `netlink_snapshot_reads_the_prepared_namespace` |
| `route_monitor` | `route_monitor_publishes_live_route_updates` |
| `route_monitor` | `route_monitor_publishes_live_neighbor_updates` |
| `route_monitor` | `route_monitor_publishes_link_removals` |

## Test Coverage

`netlink_snapshot`:

- `netlink_snapshot_reads_the_prepared_namespace`: reads interfaces, routes, and neighbors from the temporary namespace and verifies the prepared veth route and permanent neighbor are visible through netlink.

`route_monitor`:

- `route_monitor_publishes_live_route_updates`: verifies the route monitor publishes an added route with the expected next hop and later removes it after the route is deleted.
- `route_monitor_publishes_live_neighbor_updates`: verifies the route monitor publishes initial, replaced, and removed neighbor state for an existing route.
- `route_monitor_publishes_link_removals`: verifies deleting a link removes the route that depended on that link from the published router.
