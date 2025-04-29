# Provenance

Provenance is a field, not a type parameter. Multiple functions can create a Shred,
each recording where it was made.

```mermaid
flowchart LR
    wire["wire bytes"] -->|"parse_turbine"| received["Received(Turbine)<br/>Received(Repair)"]
    wire -->|"parse_repair"| received
    partial["Partial FEC set"] -->|"recover"| recovered["Recovered"]
    column["blockstore column"] -->|"from_blockstore"| stored["Blockstore"]
    entries["serialized entries"] -->|"FecSet::build"| produced["BlockProduction"]

    received --> batch["insert batch"]
    recovered --> batch
    stored --> batch
    produced --> batch
```

Only one operation currently checks provenance, and it is resigning - only received shreds
need to be resigned.
