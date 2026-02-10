# XDP Compatibility

This crate provides a minimal client/server check for Agave XDP compatibility.
If the client succeeds, XDP should work with your validator. If it fails, there
is likely a host/network/XDP issue.

Please report failures in the Solana Discord in `#learning-corner`, with your host details and command output.

## Usage

### Server (non-XDP echo)

Run on the server host:

```
cargo run -p xdp-compatability --bin server -- --bind-address 0.0.0.0:<TARGET_IP>
```

### Client (XDP sender)

For XDP zero-copy, the client binary needs extra capabilities. Build it and set caps:
```
cargo build -p xdp-compatability --bin client
sudo setcap cap_net_admin,cap_net_raw,cap_bpf,cap_perfmon+ep <path-to-client-binary>
```

Then run:
```
cargo run -p xdp-compatability --bin client -- \
  --target-server <SERVER_IP>:<TARGET_IP> \
  --experimental-retransmit-xdp-cpu-cores <CORE> \
  --experimental-retransmit-xdp-interface <IFACE> \
  --count 100 \
  --timeout-ms 1000 \
  --experimental-retransmit-xdp-zero-copy
```
