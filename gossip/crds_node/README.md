# CRDS Node

`crds_node` crate provides a lightweight, standalone implementation of a Solana
gossip/CRDS (Conflict-free Replicated Data Store) node. It offers direct control
over the messages being sent, making it ideal for testing, experimentation, and scripting.

This tool is particularly useful for developers looking to explore or test gossip behaviors in isolation.

## Features

- **Standalone Operation**: Run a CRDS node independently, without requiring the full Solana validator stack.
- **Message Injection**: Supply messages via CLI arguments or pipe them through `STDIN` as JSON-serialized input.
- **Script-Friendly**: Easily integrate with testing frameworks or automation scripts.
- **Lightweight & Flexible**: Minimal dependencies and resource usage, designed for protocol experimentation at scale within environments such as mininet.

## Caveats

This is an internal dev tool. Use it at your own risk.

- The external interface of this program is not stable and should not be relied upon.
- Command line arguments, set of messages and output format may change without prior notice.
