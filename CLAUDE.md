# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Agave is the Anza-maintained Solana validator client implementation, written in Rust. It's a high-performance blockchain architecture with extensive testing, benchmarking, and optimization focus.

## Build Commands

```bash
# Debug build (not suitable for production)
./cargo build

# Full build with all features
cargo build --workspace --all-targets --features dummy-for-ci-check,frozen-abi

# Release build (for production use)
cargo build --release
```

## Testing Commands

```bash
# Run all tests
./cargo test

# Run specific workspace tests
cargo test --workspace --exclude <package>

# Run tests with all features
cargo test --all-features

# Run benchmarks (requires nightly)
cargo +nightly bench
```

## Linting and Code Quality

```bash
# Run clippy (use the provided script for CI compliance)
./scripts/cargo-clippy.sh

# Manual clippy with all CI flags
cargo +nightly clippy --workspace --all-targets --features dummy-for-ci-check,frozen-abi -- \
  --deny=warnings \
  --deny=clippy::default_trait_access \
  --deny=clippy::arithmetic_side_effects \
  --deny=clippy::manual_let_else \
  --deny=clippy::uninlined-format-args \
  --deny=clippy::used_underscore_binding

# Format code
cargo fmt --all

# Check code coverage
./scripts/coverage.sh
```

## Architecture Overview

### Core Components

- **`core/`** - Main validator implementation containing banking stage, consensus, TPU (Transaction Processing Unit), TVU (Transaction Validation Unit), and networking components
- **`validator/`** - Validator entry point and CLI interface
- **`runtime/`** - Transaction processing runtime and account management
- **`ledger/`** - Blockchain storage and ledger management
- **`accounts-db/`** - Account storage and indexing system
- **`svm/`** - Solana Virtual Machine components
- **`rpc/`** - JSON RPC server implementation
- **`streamer/`** - Network packet streaming and processing
- **`turbine/`** - Block propagation protocol
- **`gossip/`** - Cluster information and gossip protocol

### Key Architectural Patterns

- **Banking Stage**: Central transaction processing pipeline with parallel execution
- **TPU/TVU Split**: Transaction Processing Unit handles new transactions, Transaction Validation Unit handles block validation
- **Pipelining**: Extensive use of multi-stage pipelines for performance
- **QUIC**: Modern transport protocol for improved networking
- **Proof of History (PoH)**: Built-in time source for transaction ordering

### Programs and Interfaces

- **`programs/`** - Built-in Solana programs (system, vote, stake, compute budget, etc.)
- **`syscalls/`** - System call interface for on-chain programs
- **`builtins/`** - Built-in program implementations

## Development Guidelines

### Pull Request Process

- Keep PRs under 1,000 lines for functional changes
- Use Draft PRs to run CI before requesting review
- All changes require 90%+ test coverage
- Benchmark performance-impacting changes
- Follow existing code conventions and patterns

### Code Standards

- Use `rustfmt` for formatting (latest version)
- All code must pass `clippy` with the repository's strict settings
- Variable names should be spelled out, not abbreviated
- Function names use `<verb>_<subject>` pattern
- Test functions start with `test_`, benchmarks with `bench_`

### Testing Requirements

- Unit tests for all new functionality
- Integration tests for complex flows
- Stress testing for performance-critical paths
- All tests must be deterministic and fast

### Development Workflow

1. Build understanding by reading surrounding code and imports
2. Follow existing patterns and conventions in the module
3. Run tests frequently during development
4. Use provided scripts for consistent tooling
5. Benchmark any performance-sensitive changes

## Special Files and Tools

- **`Cargo.toml`** - Workspace configuration with 140+ crates
- **`clippy.toml`** - Clippy configuration with custom rules and disallowed methods
- **`scripts/`** - Utility scripts for development and deployment
- **`ci/`** - Continuous integration configuration
- **`docs/`** - Technical documentation and design proposals

## Performance Focus

This codebase prioritizes performance with:
- Extensive benchmarking requirements
- Custom memory management patterns
- SIMD optimizations where applicable
- Careful attention to allocation patterns
- Profile-guided optimization support

## Security Considerations

- Consensus-breaking changes require feature gates and SIMDs (Solana Improvement Documents)
- Never use `unwrap()` without proof it cannot panic; prefer `expect()` with clear messages
- Follow secure coding practices for financial software
- All network-facing code undergoes security review