# Repository guidance for coding agents

## Cargo package names

This workspace contains both `agave-*` and `solana-*` packages. Do not infer a
package name from its directory. The authoritative Cargo package name is the
`name` field in the crate's `[package]` table.

Before running a package-scoped Cargo command, verify the name in the manifest.
