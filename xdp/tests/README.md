# XDP Integration Tests

These tests are run through `cargo xtask xdp-test`.

Local mode runs the tests directly on the host and requires root or equivalent network admin privileges because the harness creates a temporary network namespace, `veth` interfaces, routes, and neighbors:

```bash
cargo xtask xdp-test local --runner "sudo -n -E"
```

VM mode boots a QEMU guest and runs the same tests inside that guest:

```bash
cargo xtask xdp-test fetch-kernels --kernel-set pr
cargo xtask xdp-test vm --kernel-set pr
```

Requirements for VM mode:

- `qemu-system-x86_64` must be installed and available on `PATH`
- or pass an explicit QEMU path with `--qemu /full/path/to/qemu-system-x86_64`

If QEMU is not installed, `cargo xtask xdp-test vm ...` will fail before booting the guest.
