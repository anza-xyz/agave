# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is the `runtime` crate within the Agave (Solana) blockchain validator implementation. The runtime crate provides the core execution environment for processing transactions and managing blockchain state. It implements the Bank, which is the central component responsible for transaction processing, account management, and state transitions in the Solana validator.

## Build Commands

The project uses a custom `cargo` wrapper script located at `../cargo`. All cargo commands should be executed from the root Agave directory (`../`):

```bash
# Build the runtime crate
../cargo build -p solana-runtime

# Build with release optimizations
../cargo build -p solana-runtime --release

# Run tests for this crate specifically  
../cargo test -p solana-runtime

# Run a specific test
../cargo test -p solana-runtime test_name

# Run benchmarks (requires nightly rust)
../cargo +nightly bench -p solana-runtime

# Check for clippy linting issues
../cargo clippy -p solana-runtime

# Format code
../cargo fmt -p solana-runtime
```

## Architecture Overview

### Core Components

- **Bank** (`src/bank.rs`): The central transaction processing engine that tracks client accounts and program execution state
- **BankForks** (`src/bank_forks.rs`): DAG structure managing multiple bank checkpoints representing different blockchain forks
- **AccountsBackgroundService** (`src/accounts_background_service.rs`): Background service for cleaning up dead slots and managing account storage
- **SnapshotUtils** (`src/snapshot_utils.rs`): Utilities for creating, managing, and restoring blockchain state snapshots
- **PrioritizationFee** (`src/prioritization_fee.rs` & `prioritization_fee_cache.rs`): Transaction fee prioritization and caching mechanisms
- **EpochStakes** (`src/epoch_stakes.rs`): Management of validator stake information across epochs

### Transaction Processing Architecture

The runtime follows this high-level transaction processing flow:

1. **Transaction Entry**: Transactions enter through `Bank::process_transactions`
2. **Account Loading**: References are loaded from the accounts database 
3. **Program Execution**: Programs are loaded and executed via InvokeContext
4. **State Storage**: Results are committed back to account storage
5. **Status Tracking**: Transaction status can be queried via `get_signature_status`

### Bank Lifecycle

Banks progress through distinct lifecycle stages:

1. **Open**: Newly created bank accepting transactions
2. **Complete**: All transactions for the slot have been applied  
3. **Frozen**: No more transactions accepted, rent applied, sysvars updated
4. **Rooted**: Finalized state that cannot be removed from the chain

### Key Features

- **Fork Management**: BankForks maintains a DAG of bank checkpoints for handling blockchain forks
- **Snapshot System**: Complete state snapshots for fast bootstrap and recovery
- **Background Processing**: Dedicated service for account cleanup and maintenance
- **Fee Prioritization**: Advanced fee caching and prioritization for transaction ordering
- **Stake Management**: Comprehensive validator stake tracking across epochs
- **Dependency Tracking**: Transaction dependency resolution for parallel execution

## Development Features

The crate supports conditional compilation features:
- `dev-context-only-utils`: Enables development and testing utilities (required for tests)
- `frozen-abi`: Enables ABI freezing for production builds

## Testing

Tests are organized across several locations:
- Unit tests colocated with source files
- Integration tests in module subdirectories
- Benchmarks in `benches/` directory using criterion

## Important Implementation Details

- **Single Leader Processing**: Each bank processes transactions for a single slot from a single leader
- **Parent Relationships**: All banks except genesis point to a parent bank
- **Concurrent Access**: Supports concurrent reads while maintaining single-threaded writes
- **State Consistency**: Careful synchronization ensures consistent state across concurrent operations
- **Memory Management**: Efficient memory usage patterns for handling large blockchain state

### Snapshot Architecture

The runtime implements a comprehensive snapshot system:
- **Full Snapshots**: Complete blockchain state at specific slots
- **Incremental Snapshots**: Delta changes since the last full snapshot
- **Archive Management**: Compressed snapshot storage and retrieval
- **Rebuilding**: Ability to reconstruct account storage from snapshots

### Background Services

- **Account Cleanup**: Removes outdated account versions and reclaims space
- **Snapshot Creation**: Periodic state snapshots for recovery and bootstrapping  
- **Stats Collection**: Performance metrics and monitoring data

## Performance Considerations

- **Parallel Transaction Processing**: Uses dependency tracking for concurrent execution
- **Memory Mapping**: Efficient account storage access via memory-mapped files
- **Caching**: Aggressive caching of frequently accessed data
- **Background Operations**: Non-blocking maintenance operations

## Clippy Configuration

The project uses custom clippy rules defined in `../clippy.toml`. Always run clippy before submitting changes to ensure code quality and consistency.

## Dependencies

The crate has extensive dependencies on other Solana components:
- `solana-accounts-db`: Account storage and indexing
- `solana-svm`: Solana Virtual Machine for program execution  
- `solana-program-runtime`: Runtime environment for on-chain programs
- `solana-unified-scheduler-logic`: Advanced transaction scheduling