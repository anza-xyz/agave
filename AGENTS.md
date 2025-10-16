# Repository Guidelines

## Project Structure & Module Organization
Agave is a Rust workspace of validator, client, and tooling crates. Core execution lives in `runtime/` and `core/`, networking flows through `rpc/`, `banks-server/`, and `banks-client/`, and user-facing binaries ship from `cli/` and `cli-config/`. Shared SDK code is in `sdk/`, on-chain examples and SBF helpers live in `programs/`, `cargo-build-sbf/`, and `cargo-test-sbf/`. Reference material sits in `docs/`, automation resides in `scripts/` and `ci/`, and each crate carries unit tests beside `src/` plus broader scenarios under `tests/` or `benches/` (for example `runtime/tests/`, `local-cluster/tests/`).

## Build, Test, and Development Commands
- `./cargo build --workspace` compiles every crate using the pinned toolchain in `rust-toolchain.toml`.
- `./cargo test --workspace --all-features` runs the suite; narrow scope with `-p <crate>` during iteration.
- `./cargo clippy --workspace --all-targets` enforces lint policy and must be clean before review.
- `cargo fmt --all` (or `./cargo fmt`) applies the shared `rustfmt.toml` formatting.

## Coding Style & Naming Conventions
Follow the defaults captured in `rustfmt.toml`: four-space indentation, 100-column targets, trailing commas on multiline structures, and `snake_case` paths. Types and traits use `UpperCamelCase`; constants use `SCREAMING_SNAKE_CASE`. Prefer `expect("context")` over bare `unwrap()`, keep error messages actionable, and hide consensus-affecting work behind feature gates named after the tracking issue.

## Testing Guidelines
Unit tests rely on Rust’s built-in harness; add them beside the code path they protect. Integration and validator flows belong in dedicated crates such as `program-test/` or `local-cluster/`. Generate coverage with `scripts/coverage.sh` and attach notable deltas when coverage shifts. Benchmarks that require nightly must run via `cargo +nightly bench`, with summaries linked in the PR when performance is impacted.

## Commit & Pull Request Guidelines
Write imperative, single-line commit titles (`Add vote packet filter`). Keep commits review-sized and avoid mixing refactors with behaviour changes. Open draft PRs to trigger CI, then provide a concise problem statement, proposed changes list, test evidence, and relevant metrics or benchmarks. Link issues, refresh `CHANGELOG.md` when user-facing, and document feature gating or SIMD references for consensus changes.

## Security & Configuration Tips
Report vulnerabilities through the channel defined in `SECURITY.md` before disclosure. For reproducible validator builds, follow `docs/src/cli/install.md` and the perf-library scripts under `scripts/`. Store validator keys outside the repository and prefer hardware or `remote-wallet/` tooling for signing. Monitor `metrics/` and `watchtower/` outputs after enabling new gates to catch regressions quickly.
