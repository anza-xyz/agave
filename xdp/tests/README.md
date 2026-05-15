# XDP Integration Tests

These tests are run through `cargo xtask xdp-test`.

The default suite currently runs:

- `netlink_snapshot`
- `route_monitor`
- `router_snapshot`
- `transmitter_smoke`
- `zero_copy_hardware`

Local mode runs the tests directly on the host and requires root or equivalent network admin privileges because the harness creates a temporary network namespace, `veth` interfaces, routes, and neighbors:

```bash
cargo xtask xdp-test local --runner "sudo -n -E"
```

The harness also creates GRE tunnel interfaces for GRE route and transmit coverage.

The `zero_copy_hardware` test is opt-in. By default it skips so the portable veth
suite can run on ordinary CI workers. Hardware zero-copy CI must run on a worker with
a dedicated zero-copy-capable NIC or VF and set:

```bash
AGAVE_XDP_ZC_INTERFACE=<interface> cargo xtask xdp-test local --runner "sudo -n -E"
```

When `AGAVE_XDP_ZC_INTERFACE` is set, failing to bind queue `0` with `XDP_ZEROCOPY`,
failing to report `XDP_OPTIONS_ZEROCOPY`, or failing to build the zero-copy transmitter
is a test failure.

VM mode boots a QEMU guest and runs the same tests inside that guest:

```bash
cargo xtask xdp-test fetch-kernels --kernel-set pr
cargo xtask xdp-test vm --kernel-set pr
```

You can override the default kernel lists without editing the code. For example, to replace the default PR kernel set for a single run:

```bash
cargo xtask xdp-test fetch-kernels --kernel-set pr --pr-kernel-version 6.6 --pr-kernel-version 6.12
cargo xtask xdp-test vm --kernel-set pr --pr-kernel-version 6.6 --pr-kernel-version 6.12
```

Requirements for VM mode:

- `qemu-system-x86_64` must be installed and available on `PATH`
- or pass an explicit QEMU path with `--qemu /full/path/to/qemu-system-x86_64`

If QEMU is not installed, `cargo xtask xdp-test vm ...` will fail before booting the guest.
