# XDP Compatibility

This crate provides a minimal client/server check for Agave XDP compatibility.
If the client succeeds, XDP should work with your validator. If it fails, there
is likely a host/network/XDP issue.

Please report failures in the Solana Discord in `#learning-corner`, with your host details and command output.

## Usage

### Server (non-XDP echo)

Run on the server host:

```
cargo run -p xdp-compatability --bin server -- --bind 0.0.0.0:<TARGET_IP>
```

### Client (XDP sender)

Run on the client host:
```
cargo run -p xdp-compatability --bin client -- \
  --target-server <SERVER_IP>:<TARGET_IP> \
  --experimental-retransmit-xdp-cpu-cores <CORE> \
  --experimental-retransmit-xdp-interface <IFACE> \
  --count 100 \
  --timeout-ms 1000 \
  --experimental-retransmit-xdp-zero-copy
```
