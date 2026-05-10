//! Extension hook contract for the Agave TPU pipeline.
//!
//! This is a trait-definition crate — it is **not** a runtime plugin loader.
//!
//! # Fork integration — three steps
//!
//! **1. Implement the traits you need.** Unoverridden hooks default to no-ops:
//!
//! | Trait | Default | Override when |
//! |-------|---------|---------------|
//! | [`YieldControl`] | [`NoYield`] | need to pause the scheduler while a bundle executes |
//! | [`AccountFilter`] | [`NoFilter`] | need to block accounts from non-bundle traffic |
//! | [`WriteLockView`] | [`NoLocks`] | bundle processor holds per-slot write locks |
//! | [`TipProcessor`] | [`NoTip`] | initialise tip PDAs on leader-slot transitions |
//! | [`BatchCommitPolicy`] | [`StandardCommit`] | need atomic-batch or multi-entry PoH recording |
//!
//! **2. Construct [`BankingHooks`].** The `F` type parameter is your [`AccountFilter`];
//! it is monomorphised into the packet hot path — no dynamic dispatch on filter checks:
//!
//! ```ignore
//! let hooks = BankingHooks::new(
//!     Arc::new(MyYield),
//!     Arc::new(MyFilter),   // F = MyFilter
//!     Arc::new(MyLocks),
//!     Arc::new(MyTipProcessor),
//!     Arc::new(MyBatchPolicy),
//!     TipConfig { validator_identity, ..TipConfig::default() },
//! );
//! ```
//!
//! **3. Pass to the validator.** Call
//! `Validator::new_with_exit_filtered::<MyFilter>(…, TpuPlugin::new(stages, hooks))`
//! in place of `Validator::new`. All hooks are invoked from within banking-stage threads;
//! the validator owns `BankingHooks` for the lifetime of the process.
//!
//! A runnable reference implementation lives in `tpu-plugin/src/bin/jito_solana/`.
//!
//! # Call sites in core
//!
//! | Hook | Where | Vanilla no-op |
//! |------|-------|---------------|
//! | `yield_control.should_yield` | top of each `receive_and_buffer_packets` cycle | returns `false` |
//! | `account_filter.is_blocked` | `try_handle_packet` — per packet, per account key | returns `false` |
//! | `account_lock_view.is_write_locked` | `try_handle_packet` — per packet, per account key | returns `false` |
//! | `tip_processor.process` | leader-slot transition in `SchedulerController::run` | returns `Ok(())` |
//! | `batch_commit.revert_batch_on_error` | `ExecutionFlags::all_or_nothing` in `Consumer` | `false` |
//! | `batch_commit.partition_into_entries` | PoH recording path in `Consumer` | single entry |
//!
//! # Out of scope — bundle transaction injection
//!
//! Injecting pre-verified packets directly into the banking stage (bypassing sigverify)
//! requires a new injection point not present in this PR. [`ReadLockView`] and
//! [`BundleAccountLockView`] are defined but have no call site yet; both are reserved
//! for that follow-up. Bundle injection is tracked as a separate design discussion.
//!
//! # Known limitations (v1)
//!
//! **`ValidatorConfig::filter_keys` is silently ignored by `new_with_exit_filtered`.**
//! The `--filter-keys` CLI flag is wired through `Validator::new` into a
//! [`SetAccountFilter`]; that branch is bypassed when a fork supplies its own
//! [`BankingHooks`]. Forks that need `filter_keys` behaviour must incorporate it into
//! their [`AccountFilter`] — for example by delegating to [`SetAccountFilter`] internally.
//!
//! **`BankingHooks::new` takes five positional `Arc<dyn …>` arguments.** All five are
//! type-erased, so argument transposition compiles silently. A builder API is the natural
//! v2 path; v1 keeps the surface small at the cost of positional brittleness.

mod defaults;
mod plugin;
mod traits;

pub use defaults::{NoFilter, NoLocks, NoTip, NoYield, SetAccountFilter, StandardCommit};
pub use plugin::{BankingHooks, TipConfig, TpuPlugin};
pub use traits::{
    AccountFilter, BatchCommitPolicy, LifecycleStage,
    TipContext, TipProcessor, TipProcessorError, TpuStage, WriteLockView, YieldControl,
};
// ReadLockView and BundleAccountLockView have no call site in core yet; bundle injection
// (bypassing sigverify) is deferred to a follow-up PR. Kept pub(crate) for completeness.
pub(crate) use traits::{BundleAccountLockView, ReadLockView};
