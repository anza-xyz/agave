# Ingest

A packet off a socket to a shred in an insert batch, which is
what `examples/ingest_cascade.rs` reproduces.

```mermaid
---
config:
  flowchart:
    curve: linear
---
flowchart TB
    style packet stroke: #f00
    style parsed stroke: #ff0
    style admissible stroke: #ff0
    style nonce stroke: #ff0
    style verified stroke: #0f0
    style column stroke: #0f0
    packet["Bytes from the wire"] --> fn_parse[/"Shred::parse_repair(bytes)"/]
    fn_parse -->|"shred body"| parsed["AnyShred&lt;Parsed&gt;<br/>Prov:Received(Repair)"]
    fn_parse -.->|"repair nonce"| nonce["BlockLocation"]
    nonce -.->batch
    subgraph socket["Socket thread"]
    parsed -->|"into_data / into_code"| typed["Shred&lt;K, Parsed&gt;"]
    typed -->|"check_policy(policy)"| admissible["Shred&lt;K, Admissible&gt;"]
    end
    subgraph sigverify["Sigverify worker"]
    dedup{{"dedup on payload bytes"}}
    dedup -->|"not seen before"| fn_verify[/"verify(leader)"/]
    fn_verify --> verified["Shred&lt;K, Verified&gt;"]
    end
    admissible -->|"channel"| dedup
    verified -->|"resign(keypair)"| verified
    verified --> batch["insert batch"]
    column("blockstore column") -->|from_blockstore| batch
    batch -->|"recover(data, code)"| rebuilt["Recovery, Provenance::Recovered"]
    rebuilt --> batch
```

Policy and signature are two transitions rather than one because the work between them is what
decides whether the signature check happens at all: the batch crosses a channel and duplicates are
dropped first. Deduplication is keyed on the whole payload, so it is sound on unverified bytes.
