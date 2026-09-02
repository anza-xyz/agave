# Egress

Entries broadcast and repair response, which is what
`examples/broadcast_egress.rs` reproduces.

```mermaid
flowchart TB
    entries["serialized entries"] -->|"coalesce"| chunk["serialized EntryBatch"]
    chunk -->|"FecSet::build(spec, data, keypair)"| set["FecSet, Provenance::BlockProduction"]
    set -.->|"merkle_root chaining"| chunk
    set -->|into_any| stream["AnyShred&lt;Verified&gt;"]
    stream --> wire["Broadcast"]
    stream --> columns["blockstore insert"]
    columns -->|from_blockstore| served["Shred&lt;K, Verified&gt;<br/>Provenance::Blockstore"]
    served -->|"into_repair_response(nonce)"| response["repair response"]
```

The last batch of a slot reserves room for a retransmitter signature in every shred, so it carries
less data than an interior one.
