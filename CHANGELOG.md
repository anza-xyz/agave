# Changelog

All notable changes to this project will be documented in this file.

Please follow the [guidance](#adding-to-this-changelog) at the bottom of this file when making changes
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
and follows a [Backwards Compatibility Policy](https://docs.anza.xyz/backwards-compatibility)

## 3.1.0
### RPC
#### Breaking
* A signature verification failure in `simulateTransaction()` or the preflight stage of `sendTransaction()` will now be attached to the simulation result's `err` property as `TransactionError::SignatureFailure` instead of being thrown as a JSON RPC API error (-32003). Applications that already guard against JSON RPC exceptions should expect signature verification errors to appear on the simulation result instead. Applications that already handle the materialization of `TransactionErrors` on simulation results can now expect to receive errors of type `TransactionError::SignatureFailure` at those verification sites.
#### Changes
* The `getProgramAccounts` RPC endpoint now returns JSON-RPC errors when malformed filters are provided (previously these malformed filters would be silently ignored and the RPC call would execute an unfiltered query).
* `PubsubClient` can now be constructed with the URI of an RPC (as a `str`, `String`, or `Uri`) as well as an `http::Request<()>`. The addition of `Request` allows you to set request headers when establishing a websocket connection with an RPC.
### Validator
#### Breaking
#### Deprecations
* The `--monitor` flag with `agave-validator exit` is now deprecated. Operators can use the `monitor` command after `exit` instead.
* The `--disable-accounts-disk-index` flag is now deprecated.
* All monorepo crates falling outside the
[backward compatibility policy](https://docs.anza.xyz/backwards-compatibility) are now
deprecated, signaling their inclusion in the Agave Unstable API. Enable the
`agave-unstable-api` crate feature to acknowledge use of an interface that may break
without warning. From v4.0.0 onward, symbols in these crates will be unavailable without
`agave-unstable-api` enabled.
* The `--dev-halt-at-slot` flag is now deprecated.

#### Changes
* The accounts index is now kept entirely in memory by default.

## 3.0.0

### RPC

#### Breaking
* Added a `slot` property to `EpochRewardsPeriodActiveErrorData`
* Added error data containing a `slot` property to `RpcCustomError::SlotNotEpochBoundary`

#### Changes
* The subscription server now prioritizes processing received messages before sending out responses. This ensures that new subscription requests and time-sensitive messages like `PING` opcodes take priority over notifications.

### Validator

#### Breaking
* When XDP is enabled, the validator process requires the `CAP_NET_RAW`, `CAP_NET_ADMIN`, `CAP_BPF`, and `CAP_PERFMON` capabilities. These can be configured in the systemd service file by setting `CapabilityBoundingSet=CAP_NET_RAW CAP_NET_ADMIN CAP_BPF CAP_PERFMON` under the `[Service]` section or directly on the binary with the command `sudo setcap cap_net_raw,cap_net_admin,cap_bpf,cap_perfmon=p <path/to/agave-validator>` (this command must be run each time the binary is replaced)
* Enabling XDP zero copy on systems configured with LACP bond requires manually passing  `--experimental-retransmit-xdp-interface <real-interface>` (e.g.: `eno17395np0` not `bond0`), as zero copy is only available on physical interfaces.
* Require increased `memlock` limits - recommended setting is `LimitMEMLOCK=2000000000` in systemd service configuration. Lack of sufficient limit (on Linux) will cause startup error.
* Remove deprecated arguments
  * `--accounts-index-memory-limit-mb`
  * `--accountsdb-repl-bind-address`, `--accountsdb-repl-port`, `--accountsdb-repl-threads`, `--enable-accountsdb-repl`
  * `--disable-quic-servers`, `--enable-quic-servers`
  * `--etcd-cacert-file`, `--etcd-cert-file`, `--etcd-domain-name`, `--etcd-endpoint`, `--etcd-key-file`, `--tower-storage`
  * `--no-check-vote-account`
  * `--no-rocksdb-compaction`, `--rocksdb-compaction-interval-slots`, `--rocksdb-max-compaction-jitter-slots`
  * `--replay-slots-concurrently`
    * Use `--replay-forks-threads` with a value of `4` to match preexisting behavior
  * `--rpc-pubsub-max-connections`, `--rpc-pubsub-max-fragment-size`, `--rpc-pubsub-max-in-buffer-capacity`, `--rpc-pubsub-max-out-buffer-capacity`, `--enable-cpi-and-log-storage`, `--minimal-rpc-api`
  * `--skip-poh-verify`
* Deprecated snapshot archive formats have been removed and are no longer loadable.
* Using `--snapshot-interval-slots 0` to disable generating snapshots has been removed. Use `--no-snapshots` instead.
* Validator will now bind all ports within provided `--dynamic-port-range`, including the client ports. A range of at least 25 ports is recommended to avoid failures to bind during startup.
* Agave and agave-ledger-tool can no longer operate with legacy shreds. Legacy shreds have not been in circulation since the activation of https://explorer.solana.com/address/GV49KKQdBNaiv2pgqhS2Dy3GWYJGXMTVYbYkdk91orRy. This change may break operations with old ledgers that may still contain legacy shreds.

#### Changes
* `--transaction-structure view` is now the default.
* The default full snapshot interval is now 100,000 slots.
* `SOLANA_BANKING_THREADS` environment variable is no longer supported. Use `--block-prouduction-num-workers` instead.
* By default, `agave-validator exit` will now wait for the validator process to terminate before returning. The `--wait-for-exit` flag has been deprecated, but operators can still opt out with the new `--no-wait-for-exit` flag.

## 2.3.0

### Validator

#### Breaking
* ABI of `TimedTracedEvent` changed, since `PacketBatch` became an enum, which carries different packet batch types. (#5646)

#### Changes
* Account notifications for Geyser are no longer deduplicated when restoring from a snapshot.
* Add `--no-snapshots` to disable generating snapshots.
* `--block-production-method central-scheduler-greedy` is now the default.
* The default full snapshot interval is now 50,000 slots.
* Graceful exit (via `agave-validtor exit`) is required in order to boot from local state. Refer to the help of `--use-snapshot-archives-at-startup` for more information about booting from local state.

#### Deprecations
* Using `--snapshot-interval-slots 0` to disable generating snapshots is now deprecated.
* Using `blockstore-processor` for `--block-verification-method` is now deprecated.

### Platform Tools SDK

#### Changes
* `cargo-build-sbf` and `cargo-test-sbf` now accept `v0`, `v1`, `v2` and `v3` for the `--arch` argument. These parameters specify the SBPF version to build for.
* SBFPv1 and SBPFv2 are also available for Anza's C compiler toolchain.
* SBPFv3 will be only available for the Rust toolchain. The C toolchain will no longer be supported for SBPFv3 onwards.
* `cargo-build-sbf` now supports the `--optimize-size` argument, which reduces program size, potentially at the cost of increased CU usage.

#### Breaking
* Although the solana rust toolchain still supports the `sbf-solana-solana` target, the new `cargo-build-sbf` version target defaults to `sbpf-solana-solana`. The generated programs will be available on `target/deploy` and `target/sbpf-solana-solana/release`.
* If the `sbf-solana-solana` target folder is still necessary, use `cargo +solana build --triple sbf-solana-solana --release`.
* The target triple changes as well for the new SBPF versions. Triples will be `sbpfv1-solana-solana` for version `v1`, `sbpfv2-solana-solana` for `v2`, and `sbpfv3-solana-solana` for `v3`. Generated programs are available on both the `target/deploy` folder and the `target/<triple>/release` folder. The binary in `target/deploy` has smaller size, since we strip unnecessary sections from the one available in `target/<triple>/release`.
* `cargo-build-sbf` no longer automatically enables the `program` feature to the `solana-sdk` dependency. This feature allowed `solana-sdk` to work in on-chain programs. Users must enable the `program` feature explicitly or use `solana-program` instead. This new behavior only breaks programs using `solana-sdk` v1.3 and earlier.

### CLI

#### Changes
* `withdraw-stake` now accepts the `AVAILABLE` keyword for the amount, allowing withdrawal of unstaked lamports (#4483)
* `solana-test-validator` will now bind to localhost (127.0.0.1) by default rather than all interfaces to improve security. Provide `--bind-address 0.0.0.0` to bind to all interfaces to restore the previous default behavior.

### RPC

#### Changes
* `simulateTransaction` now includes `loadedAccountsDataSize` in its result. `loadedAccountsDataSize` is the total number of bytes loaded for all accounts in the simulated transaction.

## 2.2.0

### CLI

#### Changes
* Add global `--skip-preflight` option for skipping preflight checks on all transactions sent through RPC. This flag, along with `--use-rpc`, can improve success rate with program deployments using the public RPC nodes.
* Add new command `solana feature revoke` for revoking pending feature activations. When a feature is activated, `solana feature revoke <feature-keypair> <cluster>` can be used to deallocate and reassign the account to the System program, undoing the operation. This can only be done before the feature becomes active.

### Validator

#### Breaking
* Blockstore Index column format change
  * The Blockstore Index column format has been updated. The column format written in v2.2 is compatible with v2.1, but incompatible with v2.0 and older.
* Snapshot format change
  * The snapshot format has been modified to implement SIMD-215. Since only adjacent versions are guaranteed to maintain snapshot compatibility, this means snapshots created with v2.2 are compatible with v2.1 and incompatible with v2.0 and older.

#### Changes
* Add new variant to `--block-production-method` for `central-scheduler-greedy`. This is a simplified scheduler that has much better performance than the more strict `central-scheduler` variant.
* Unhide `--accounts-db-access-storages-method` for agave-validator and agave-ledger-tool and change default to `file`
* Remove tracer stats from banking-trace. `banking-trace` directory should be cleared when restarting on v2.2 for first time. It will not break if not cleared, but the file will be a mix of new/old format. (#4043)
* Add `--snapshot-zstd-compression-level` to set the compression level when archiving snapshots with zstd.

#### Deprecations
* Deprecate `--tower-storage` and all `--etcd-*` arguments

### SDK

#### Changes
* `cargo-build-sbf`: add `--skip-tools-install` flag to avoid downloading platform tools and `--no-rustup-override` flag to not use rustup when invoking `cargo`. Useful for immutable environments like Nix.

## 2.1.0
* Breaking:
  * SDK:
    * `cargo-build-bpf` and `cargo-test-bpf` have been deprecated for two years and have now been definitely removed.
       Use `cargo-build-sbf` and `cargo-test-sbf` instead.
    * dependency: `curve25519-dalek` upgraded to new major version 4 (#1693). This causes breakage when mixing v2.0 and v2.1 Solana crates, so be sure to use all of one or the other. Please use only crates compatible with v2.1.
  * Stake:
    * removed the unreleased `redelegate` instruction processor and CLI commands (#2213)
  * Banks-client:
    * relax functions to use `&self` instead of `&mut self` (#2591)
  * `agave-validator`:
    * Remove the deprecated value of `fifo` for `--rocksdb-shred-compaction` (#3451)
* Changes
  * SDK:
    * removed the `respan` macro. This was marked as "internal use only" and was no longer used internally.
    * add `entrypoint_no_alloc!`, a more performant program entrypoint that avoids allocations, saving 20-30 CUs per unique account
    * `cargo-build-sbf`: a workspace or package-level Cargo.toml may specify `tools-version` for overriding the default platform tools version when building on-chain programs. For example:
```toml
[package.metadata.solana]
tools-version = "1.43"
```
or
```toml
[workspace.metadata.solana]
tools-version = "1.43"
```
The order of precedence for the chosen tools version goes: `--tools-version` argument, package version, workspace version, and finally default version.
  * `package-metadata`: specify a program's id in Cargo.toml for easy consumption by downstream users and tools using `solana-package-metadata` (#1806). For example:
```toml
[package.metadata.solana]
program-id = "MyProgram1111111111111111111111111111111111"
```
Can be consumed in the program crate:
```rust
solana_package_metadata::declare_id_with_package_metadata!("solana.program-id");
```
This is equivalent to writing:
```rust
solana_pubkey::declare_id!("MyProgram1111111111111111111111111111111111");
```
  * `agave-validator`: Update PoH speed check to compare against current hash rate from a Bank (#2447)
  * `solana-test-validator`: Add `--clone-feature-set` flag to mimic features from a target cluster (#2480)
  * `solana-genesis`: the `--cluster-type` parameter now clones the feature set from the target cluster (#2587)
  * `unified-scheduler` as default option for `--block-verification-method` (#2653)
  * warn that `thread-local-multi-iterator` option for `--block-production-method` is deprecated (#3113)

## 2.0.0
* Breaking
  * SDK:
    * Support for Borsh v0.9 removed, please use v1 or v0.10 (#1440)
    * `Copy` is no longer derived on `Rent` and `EpochSchedule`, please switch to using `clone()` (solana-labs#32767)
    * `solana-sdk`: deprecated symbols removed
    * `solana-program`: deprecated symbols removed
  * RPC: obsolete and deprecated v1 endpoints are removed. These endpoints are:
    confirmTransaction, getSignatureStatus, getSignatureConfirmation, getTotalSupply,
    getConfirmedSignaturesForAddress, getConfirmedBlock, getConfirmedBlocks, getConfirmedBlocksWithLimit,
    getConfirmedTransaction, getConfirmedSignaturesForAddress2, getRecentBlockhash, getFees,
    getFeeCalculatorForBlockhash, getFeeRateGovernor, getSnapshotSlot getStakeActivation
  * Deprecated methods are removed from `RpcClient` and `RpcClient::nonblocking`
  * `solana-client`: deprecated re-exports removed; please import `solana-connection-cache`, `solana-quic-client`, or `solana-udp-client` directly
  * Deprecated arguments removed from `agave-validator`:
    * `--enable-rpc-obsolete_v1_7` (#1886)
    * `--accounts-db-caching-enabled` (#2063)
    * `--accounts-db-index-hashing` (#2063)
    * `--no-accounts-db-index-hashing` (#2063)
    * `--incremental-snapshots` (#2148)
    * `--halt-on-known-validators-accounts-hash-mismatch` (#2157)
* Changes
  * `central-scheduler` as default option for `--block-production-method` (#34891)
  * `solana-rpc-client-api`: `RpcFilterError` depends on `base64` version 0.22, so users may need to upgrade to `base64` version 0.22
  * Changed default value for `--health-check-slot-distance` from 150 to 128
  * CLI: Can specify `--with-compute-unit-price`, `--max-sign-attempts`, and `--use-rpc` during program deployment
  * RPC's `simulateTransaction` now returns an extra `replacementBlockhash` field in the response
    when the `replaceRecentBlockhash` config param is `true` (#380)
  * SDK: `cargo test-sbf` accepts `--tools-version`, just like `build-sbf` (#1359)
  * CLI: Can specify `--full-snapshot-archive-path` (#1631)
  * transaction-status: The SPL Token `amountToUiAmount` instruction parses the amount into a string instead of a number (#1737)
  * Implemented partitioned epoch rewards as per [SIMD-0118](https://github.com/solana-foundation/solana-improvement-documents/blob/fae25d5a950f43bd787f1f5d75897ef1fdd425a7/proposals/0118-partitioned-epoch-reward-distribution.md). Feature gate: #426. Specific changes include:
    * EpochRewards sysvar expanded and made persistent (#428, #572)
    * Stake Program credits now allowed during distribution (#631)
    * Updated type in Bank::epoch_rewards_status (#1277)
    * Partitions are recalculated on boot from snapshot (#1159)
    * `epoch_rewards_status` removed from snapshot (#1274)
  * Added `unified-scheduler` option for `--block-verification-method` (#1668)
  * Deprecate the `fifo` option for `--rocksdb-shred-compaction` (#1882)
    * `fifo` will remain supported in v2.0 with plans to fully remove in v2.1

## 1.18.0
* Changes
  * Added a github check to support `changelog` label
  * The default for `--use-snapshot-archives-at-startup` is now `when-newest` (#33883)
    * The default for `solana-ledger-tool`, however, remains `always` (#34228)
  * Added `central-scheduler` option for `--block-production-method` (#33890)
  * Updated to Borsh v1
  * Added allow_commission_decrease_at_any_time feature which will allow commission on a vote account to be
    decreased even in the second half of epochs when the commission_updates_only_allowed_in_first_half_of_epoch
    feature would have prevented it
  * Updated local ledger storage so that the RPC endpoint
    `getSignaturesForAddress` always returns signatures in block-inclusion order
  * RPC's `simulateTransaction` now returns `innerInstructions` as `json`/`jsonParsed` (#34313).
  * Bigtable upload now includes entry summary data for each slot, stored in a
    new `entries` table
  * Forbid multiple values for the `--signer` CLI flag, forcing users to specify multiple occurrences of `--signer`, one for each signature
  * New program deployments default to the exact size of a program, instead of
    double the size. Program accounts must be extended with `solana program extend`
    before an upgrade if they need to accommodate larger programs.
  * Interface for `gossip_service::get_client()` has changed. `gossip_service::get_multi_client()` has been removed.
  * CLI: Can specify `--with-compute-unit-price`, `--max-sign-attempts`, and `--use-rpc` during program deployment
* Upgrade Notes
  * `solana-program` and `solana-sdk` default to support for Borsh v1, with
limited backward compatibility for v0.10 and v0.9. Please upgrade to Borsh v1.
  * Operators running their own bigtable instances need to create the `entries`
    table before upgrading their warehouse nodes

## 1.17.0
* Changes
  * Added a changelog.
  * Added `--use-snapshot-archives-at-startup` for faster validator restarts
* Upgrade Notes

## Adding to this Changelog
### Audience
* Entries in this log are intended to be easily understood by contributors,
consensus validator operators, rpc operators, and dapp developers.

### Noteworthy
* A change is noteworthy if it:
  * Adds a feature gate, or
  * Implements a SIMD, or
  * Modifies a public API, or
  * Changes normal validator / rpc run configurations, or
  * Changes command line arguments, or
  * Fixes a bug that has received public attention, or
  * Significantly improves performance, or
  * Is authored by an external contributor.

### Instructions
* Update this log in the same pull request that implements the change. If the
change is spread over several pull requests update this log in the one that
makes the feature code complete.
* Add notes to the [Unreleased] section in each branch that you merge to.
  * Add a description of your change to the Changes section.
  * Add Upgrade Notes if the change is likely to require:
    * validator or rpc operators to update their configs, or
    * dapp or client developers to make changes.
* Link to any relevant feature gate issues or SIMDs.
* If you add entries on multiple branches use the same wording if possible.
This simplifies the process of diffing between versions of the log.

## Maintaining This Changelog
### When creating a new release branch:
* Commit to master updating the changelog:
  * Update the edge, beta, and stable links
  * Create new section: `vx.y+1.0 - Unreleased`
  * Remove `Unreleased` annotation from vx.y.0 section.
* Create vx.y branch starting at that commit.
* Commit to `vx.y` updating the changelog:
  * Remove the `vx.y+1.0 - Unreleased` section
  * Remove the channel links
* Tag vx.y.0 on the new branch
