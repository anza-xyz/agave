# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is the `accounts-db` crate within the Agave (Solana) blockchain validator implementation. This crate handles persistent storage and management of account data, implementing a high-performance accounts database with concurrent reads and sequential writes.

## Build Commands

The project uses a custom `cargo` wrapper script located at `../../cargo`. All cargo commands should be executed from the root Agave directory (`../../`):

```bash
# Build the accounts-db crate
../../cargo build -p solana-accounts-db

# Build with release optimizations
../../cargo build -p solana-accounts-db --release

# Run tests for this crate specifically  
../../cargo test -p solana-accounts-db

# Run a specific test
../../cargo test -p solana-accounts-db test_name

# Run benchmarks (requires nightly rust)
../../cargo +nightly bench -p solana-accounts-db

# Check for clippy linting issues
../../cargo clippy -p solana-accounts-db

# Format code
../../cargo fmt -p solana-accounts-db
```

## Architecture Overview

### Core Components

- **AccountsDb** (`src/accounts_db.rs`): Main database implementation providing account storage, indexing, and retrieval
- **AccountsIndex** (`src/accounts_index.rs`): In-memory index mapping account pubkeys to storage locations
- **AccountStorage** (`src/account_storage.rs`): Manages individual storage files (AppendVecs and TieredStorage)
- **AppendVec** (`src/append_vec.rs`): Legacy append-only storage format for accounts
- **TieredStorage** (`src/tiered_storage/`): Next-generation storage format with better compression and read performance

### Storage Architecture

The accounts database uses a multi-layered storage approach:

1. **Memory Cache** (`accounts_cache.rs`): Recently accessed accounts kept in memory
2. **Storage Files**: Persistent storage using either AppendVec or TieredStorage formats
3. **Index**: Maps account pubkeys to their storage locations and metadata

### Key Features

- **Concurrent Access**: Supports many concurrent readers with single-threaded writes
- **Memory Mapping**: Storage files are memory-mapped for efficient access
- **Compression**: TieredStorage provides LZ4 compression for space efficiency
- **Background Processing**: Cleaning, shrinking, and compaction run in background threads

## Development Features

The crate supports conditional compilation features:
- `dev-context-only-utils`: Enables development and testing utilities (required for tests)
- `frozen-abi`: Enables ABI freezing for production builds

## Testing

Tests are organized in several files:
- Unit tests are colocated with source files
- Integration tests in `tests/` directory  
- Comprehensive test suite in `src/accounts_db/tests.rs`

The crate uses criterion for benchmarking performance-critical code paths.

## Important Implementation Details

- **Write Versioning**: Uses atomic write version counter for ordering commits across the entire database
- **Slot-Based Storage**: Each AppendVec stores accounts for a single slot only
- **Background Cleaning**: Removes outdated account versions and reclaims space
- **Memory Management**: Careful memory mapping and cleanup to handle large datasets efficiently

## Clippy Configuration

The project uses custom clippy rules defined in `../../clippy.toml` which disallow certain networking methods and deprecated macros. Always run clippy before submitting changes.