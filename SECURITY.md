# Security Policy

## Alpenglow Consensus Modules

**Maintained by:** Richard Patterson (@De-ASI-INTERFACE)  
**Orgs:** DeASI-INTERFACE · QuantumTradingInfinity · richy.ai  

Vulnerabilities in the Alpenglow consensus modules
(`votor`, `rotor`, `alpenglow`, `entrypoint`) should be reported
via GitHub Security Advisories to @De-ASI-INTERFACE.

Do **not** open public issues for consensus-critical bugs.

**Response SLA:** 48h triage • 7-day coordinated disclosure window.

### Known Attack Surfaces

| Surface | Mitigation | Status |
|---|---|---|
| Fake `VotePacket` injection via stake weight manipulation | BLS signature set verification | Pending SIMD-0337 |
| Relay queue flooding | `max_queue_depth` backpressure with observable counter | Implemented |
| Hop amplification attacks | `max_hops` hard ceiling with silent drop + counter | Implemented |
| Feature gate bypass | Gate defaults `false`; requires explicit governance activation | Implemented |
