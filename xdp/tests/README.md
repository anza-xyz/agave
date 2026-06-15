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
- `router_snapshot`
- `transmitter_smoke`

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

The copy-mode transmitter tests use the same primary veth pair. The transmitter binds AF_XDP TX to `axdp0`; the test binds a raw packet socket to `axdp1` and verifies the emitted Ethernet/IP/UDP frame:

```text
temporary test network namespace

  XdpSender -> copy-mode AF_XDP TX socket
        |
        v
  axdp0 10.0.0.1/24  02:aa:bb:cc:dd:01
        |
        | veth peer
        |
  axdp1 10.0.0.2/24  02:aa:bb:cc:dd:02
        ^
        |
  raw packet receiver
```

GRE tests add a tunnel on top of the primary veth pair. The transmitter sends the inner UDP packet to the overlay destination; the route resolves through `gxdp0`, the XDP transmit path wraps the packet in GRE, and the raw packet receiver observes the outer packet on `axdp1`.

```text
inner packet:
  192.0.2.1:<src-port> -> 192.0.2.99:<dst-port>

GRE overlay route:
  192.0.2.0/24 dev gxdp0 src 192.0.2.1

GRE tunnel:
  gxdp0
    local underlay:  10.0.0.1  (axdp0)
    remote underlay: 10.0.0.2  (axdp1)
    overlay source:  192.0.2.1/32
    ttl: 64

outer packet observed by receiver on axdp1:
  Ethernet: 02:aa:bb:cc:dd:01 -> 02:aa:bb:cc:dd:02
  IPv4:     10.0.0.1 -> 10.0.0.2
  GRE:      inner IPv4/UDP packet
```

## Individual Tests

Use the single-test command form above with these test binaries and names:

| Test binary | Test name |
| --- | --- |
| `netlink_snapshot` | `netlink_snapshot_reads_the_prepared_namespace` |
| `netlink_snapshot` | `netlink_snapshot_reads_gre_tunnel_metadata` |
| `route_monitor` | `route_monitor_publishes_live_route_updates` |
| `route_monitor` | `route_monitor_publishes_live_neighbor_updates` |
| `route_monitor` | `route_monitor_publishes_link_removals` |
| `route_monitor` | `route_monitor_publishes_live_gre_route_updates` |
| `router_snapshot` | `router_snapshot_resolves_gre_routes_from_netlink` |
| `transmitter_smoke` | `transmitter_sends_udp_payload_over_veth_in_copy_mode` |
| `transmitter_smoke` | `transmitter_sends_udp_payload_over_gre_tunnel_in_copy_mode` |

## Test Coverage

`netlink_snapshot`:

- `netlink_snapshot_reads_the_prepared_namespace`: reads interfaces, routes, and neighbors from the temporary namespace and verifies the prepared veth route and permanent neighbor are visible through netlink.
- `netlink_snapshot_reads_gre_tunnel_metadata`: reads a GRE tunnel interface from netlink and verifies its local endpoint, remote endpoint, TTL, and TOS metadata.

`route_monitor`:

- `route_monitor_publishes_live_route_updates`: verifies the route monitor publishes an added route with the expected next hop and later removes it after the route is deleted.
- `route_monitor_publishes_live_neighbor_updates`: verifies the route monitor publishes initial, replaced, and removed neighbor state for an existing route.
- `route_monitor_publishes_link_removals`: verifies deleting a link removes the route that depended on that link from the published router.
- `route_monitor_publishes_live_gre_route_updates`: verifies the route monitor publishes a GRE overlay route, including the underlay MAC and GRE tunnel metadata, and removes it when the GRE link is deleted.

`router_snapshot`:

- `router_snapshot_resolves_gre_routes_from_netlink`: verifies router snapshots resolve GRE overlay routes with the expected preferred source, underlay MAC, tunnel endpoints, TTL, and TOS.

`transmitter_smoke`:

- `transmitter_sends_udp_payload_over_veth_in_copy_mode`: builds the copy-mode transmitter, sends a UDP payload through `XdpSender`, and verifies the raw Ethernet/IP/UDP frame received on the peer veth.
- `transmitter_sends_udp_payload_over_gre_tunnel_in_copy_mode`: builds the copy-mode transmitter for a GRE route, sends a UDP payload through `XdpSender`, and verifies the GRE-encapsulated outer and inner packet fields.
