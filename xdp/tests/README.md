# XDP Integration Tests

These tests are marked as ignored so ordinary workspace test jobs do not run them without privileges. They are run through `cargo xtask xdp-test`.

The tests run directly on the host and require root or equivalent network admin privileges because the harness creates a temporary network namespace, `veth` interfaces, routes, and neighbors. `xtask` builds the test binaries as the current user, then applies `--runner` only when running the compiled test executables:

```bash
cargo xtask xdp-test --runner "sudo -n -E"
```

To run a single test binary locally, use this form:

```bash
cargo xtask xdp-test --runner "sudo -n -E" --test <test-binary> -- <test-name> --exact --nocapture
```

## Test Topology

Each integration test runs in a fresh temporary network namespace created with `unshare(CLONE_NEWNET)`. The tests bring `lo` up, create the interfaces needed by that test, and restore the original namespace when the test exits.

The primary topology is one veth pair inside that namespace:

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

GRE coverage adds `gxdp0` on top of the veth pair and routes the overlay prefix through that tunnel:

```text
GRE overlay route:
  192.0.2.0/24 dev gxdp0 src 192.0.2.1

GRE tunnel:
  gxdp0
    local underlay:  10.0.0.1  (axdp0)
    remote underlay: 10.0.0.2  (axdp1)
    overlay source:  192.0.2.1/32
    ttl: 64
```
