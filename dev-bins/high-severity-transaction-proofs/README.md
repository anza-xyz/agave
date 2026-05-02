# High Severity Transaction Proof Tool

This Rust tool proves that the high-severity blockchain transaction issues fixed in this branch remain guarded.

It checks:

1. Blocking durable nonce validation rejects a caller-provided hash that does not match the nonce account's stored durable nonce hash.
2. Nonblocking durable nonce validation rejects the same mismatch.
3. TPU `AsyncClient::async_send_versioned_transaction()` propagates send failure instead of returning `Ok(signature)` after ignoring a failed send.

Live mode is intentionally read-only. It never loads keypairs, signs transactions, sends transactions, spends SOL, or touches TPU sockets. It uses RPC reads only: `getVersion`, `getEpochInfo`, `getLatestBlockhash`, `getAccount`, and nonce-account validation probes.

By default, `--iterations 1000` repeats each source proof family 1000 times. Live RPC probes are chunked at one live probe per 100 iterations by default so public RPC endpoints are not abused. Use `--live-rpc-chunk 1` only with a private/local RPC endpoint if you intentionally want one live RPC probe per iteration.

Run from the Agave repo root:

```bash
cargo run --release --manifest-path dev-bins/high-severity-transaction-proofs/Cargo.toml -- --iterations 1000 --live --quiet-passes
```


If your public RPC endpoint has transient failures but you still want the command to finish after proving the offline guards, add `--live-best-effort`. This keeps live RPC failures visible in output but exits successfully only when all non-live security proofs passed:

```bash
cargo run --release --manifest-path dev-bins/high-severity-transaction-proofs/Cargo.toml -- --iterations 1000 --live --quiet-passes --live-best-effort
```

For a private RPC URL, prefer the env var so shells/logs do not expose it as a command-line argument:

```bash
HIGH_SEVERITY_PROOF_RPC_URL="https://your-rpc.example" \
  cargo run --release --manifest-path dev-bins/high-severity-transaction-proofs/Cargo.toml -- --iterations 1000 --live --quiet-passes
```

If you have a real durable nonce account on that cluster, add it for the strongest live nonce proof:

```bash
HIGH_SEVERITY_PROOF_RPC_URL="https://your-rpc.example" \
  cargo run --release --manifest-path dev-bins/high-severity-transaction-proofs/Cargo.toml -- \
    --iterations 1000 \
    --live \
    --quiet-passes \
    --live-nonce-account <DURABLE_NONCE_ACCOUNT_PUBKEY>
```

Expected good output ends with:

```text
Summary: total=3010 passed=3010 failed=0 skipped=0 ...
All enabled high severity proofs passed.
Live mode used read-only RPC calls only: ... It did not sign or send transactions.
```

If any proof says `FAIL`, the corresponding high-severity guard is missing, regressed, or the live RPC proof did not get the expected rejection.
