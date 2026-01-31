# XDP Compatibility

This crate provides a minimal client/server check for Agave XDP compatibility.
If the client succeeds, XDP should work with your validator. If it fails, there
is likely a host/network/XDP issue.

Please report failures in the Solana Discord in `#learning-corner`, with your host details and command output.

## Usage

### Server (non-XDP echo)

Run on the server host:

```
cargo run -p xdp-compatability --bin server -- --bind 0.0.0.0:9000
```

### Client (XDP sender)

Run on the client host:

XDP SKB mode:
```
cargo run -p xdp-compatability --bin client -- \
  --server <SERVER_IP>:9000 \
  --interface <IFACE> \
  --cpu 0 \
  --count 100 \
  --timeout-ms 1000
```

XDP zero-copy mode:

```
cargo run -p xdp-compatability --bin client -- \
  --server <SERVER_IP>:9000 \
  --interface <IFACE> \
  --cpu 0 \
  --count 100 \
  --timeout-ms 1000 \
  --zero-copy
```
