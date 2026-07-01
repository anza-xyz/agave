# agave-mesh-layer

An optional second network layer that uses RaptorQ fountain-code erasure
coding to replicate shreds to all validators as quickly as possible.

## Overview

Unlike the existing Turbine data plane — which organizes validators into a
tree (fanout 200, up to 4 hops) — the mesh layer **floods** RaptorQ-encoded
symbols directly to all known TVU peers over UDP.  This trades higher
bandwidth (each node sends to every peer) for lower latency (a single hop
to reach the entire cluster) and superior loss resilience.

RaptorQ is a *rateless* fountain code: from `k` source symbols it can
generate an unlimited number of repair symbols, and the decoder can
reconstruct the original data from **any** `k` received symbols.  This
means:

- Receivers don't need specific shred indices
- No coordination needed on which repair symbols to send
- Naturally handles heterogeneous loss rates across the mesh

## Data flow

```text
  shred fetch → sigverify → ┌─→ Turbine retransmit (tree)
                             └─→ DualSender → mesh forwarder
                                               │ groups by FEC set
                                               ▼
                                         mesh sender
                                         │ RaptorQ encode
                                         │ flood to all TVU peers
                                         ▼
  mesh receiver ← UDP ← (all peers)
  │ RaptorQ decode
  ▼
  mesh ingress → verified_sender → window service
```

1. **Tap**: Verified shreds are tapped from the retransmit path via a
   `DualSender` — each batch goes to both the normal Turbine retransmit
   channel and the mesh forwarder.
2. **Forwarder**: Groups shreds by FEC set (slot + fec_set_index), frames
   them, and sends `MeshBatchInput`s to the mesh sender.
3. **Sender**: RaptorQ-encodes each batch and floods source + repair
   symbols to all TVU peers via UDP.
4. **Receiver**: Listens for mesh packets, groups symbols by batch ID,
   reconstructs the original shred payloads via RaptorQ decoding.
5. **Ingress**: Converts reconstructed bytes back to `shred::Payload` and
   sends them to the TVU verified-shred pipeline.

## Configuration

The mesh layer is opt-in.  Set these fields in `ValidatorConfig`:

| Field | Default | Description |
|-------|---------|-------------|
| `enable_mesh_layer` | `false` | Enables the mesh layer service |
| `verify_mesh_shreds` | `false` | Re-verify mesh shreds through sigverify |

When `verify_mesh_shreds` is `false` (default), mesh-reconstructed shreds
bypass signature verification — they were already verified by the original
sender before entering the retransmit path.  Set to `true` for
defense-in-depth at the cost of extra sigverify CPU.

## Bandwidth / latency tradeoff

The mesh layer sends each encoded symbol to **all** TVU peers, not just a
subset of tree children.  For a cluster of `N` peers, each node sends
`N × (k + repair_symbols)` UDP datagrams per FEC set, where `k` is the
number of source symbols.  This is significantly more bandwidth than
Turbine's tree-based fanout but achieves single-hop propagation to the
entire cluster.
