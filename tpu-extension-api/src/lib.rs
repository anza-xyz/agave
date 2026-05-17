//! Extension API for Agave's TPU pipeline.
//!
//! This crate defines the contracts a downstream fork can use to add TPU-side
//! services without patching core banking code for each service. It is not a
//! runtime plugin loader.
//!
//! The default implementations preserve vanilla Agave behavior.
//!
//! # Vanilla Validators
//!
//! Validators that do not opt into TPU extensions keep calling
//! `Validator::new_with_exit(...)`. That function internally calls
//! `new_with_exit_with_tpu_extensions` with [`TpuExtensions::noop()`] —
//! `BankingHooks<NoFilter, NoGate, NoExternalLocks>` — so all hot-path hook
//! checks inline to `false` and are eliminated by the compiler. No vtable, no
//! branch, no overhead compared to pre-extension Agave.
//!
//! The only non-trivial wiring in the vanilla path is bridging
//! `ValidatorConfig::filter_keys` (the `--filter-keys` CLI flag) into
//! [`SetAccountFilter`] when the set is non-empty.
//!
//! ```ignore
//! // Equivalent to what Validator::new_with_exit does internally:
//! let extensions = if config.filter_keys.is_empty() {
//!     TpuExtensions::noop()
//! } else {
//!     TpuExtensions::builder()
//!         .banking_hooks(
//!             BankingHooks::builder()
//!                 .packet_filter((*config.filter_keys).clone().into()) // HashSet → SetAccountFilter
//!                 .build(),
//!         )
//!         .build()
//! };
//!
//! let validator = Validator::new_with_exit_with_tpu_extensions::<SetAccountFilter, NoGate, NoExternalLocks>(
//!     node,
//!     identity_keypair,
//!     ledger_path,
//!     vote_account,
//!     authorized_voter_keypairs,
//!     cluster_entrypoints,
//!     config,
//!     rpc_to_plugin_manager_receiver,
//!     start_progress,
//!     socket_addr_space,
//!     tpu_config,
//!     admin_rpc_service_post_init,
//!     xdp_builder_with_src_addr,
//!     extensions,
//!     exit,
//! )?;
//! ```
//!
//! # Fork Integration
//!
//! Build a [`BankingHooks`] value from the extension points the fork needs, then
//! pass it with any owned stages or launch-context factories through
//! [`TpuExtensions`]:
//!
//! ```ignore
//! let hooks = BankingHooks::builder()
//!     .scheduler_gate(MyGate)
//!     .packet_filter(MyFilter)
//!     .shared_external_locks(Arc::clone(&my_locks))
//!     .shared_tip_processor(Arc::clone(&my_tip_processor))
//!     .config(BankingConfig {
//!         commit_mode: BatchCommitMode::standard(),
//!     })
//!     .build();
//!
//! let extensions = TpuExtensions::builder()
//!     .with_packet_receiver()
//!     .processing_stage(TpuStageSpec::running(my_executor_stage))
//!     .intake_stage(TpuStageSpec::factory(my_ingress_factory))
//!     .banking_worker_pool(my_worker_pool_factory)
//!     .banking_hooks(hooks)
//!     .build();
//!
//! Validator::new_with_exit_with_tpu_extensions::<MyFilter, MyGate, MyLocks>(
//!     node, identity_keypair, ledger_path, vote_account,
//!     authorized_voter_keypairs, cluster_entrypoints, config,
//!     rpc_to_plugin_manager_receiver, start_progress, socket_addr_space,
//!     tpu_config, admin_rpc_service_post_init, xdp_builder_with_src_addr,
//!     extensions, exit,
//! )?;
//! ```
//!
//! # Extension Points
//!
//! | Extension point | Default | Use when |
//! |-------|---------|----------|
//! | [`SchedulerGate`] | [`NoGate`] | a sidecar stage needs the packet scheduler to pause briefly |
//! | [`AccountFilter`] | [`NoFilter`] | some accounts should be rejected before scheduling |
//! | [`ExternalLocks`] | [`NoExternalLocks`] | extension-owned in-flight work temporarily locks accounts |
//! | [`TipProcessor`] | [`NoTip`] | leader-slot transitions need fork-specific setup |
//! | [`BankingConfig::commit_mode`] | [`BatchCommitMode::standard`] | a batch should use [`BatchCommitMode::all_or_nothing`] |
//! | [`TpuStage`] | none | the fork owns an additional TPU-side service thread |
//! | [`TpuStageFactory`] | none | a stage needs Agave-created launch handles such as [`PacketIngress`] |
//! | [`TpuExtensionsBuilder::with_packet_receiver`] | direct packet flow | a fork needs to route raw TPU packets before sigverify |
//! | [`BundleExecution`] | [`NoBundleExecution`] | an extension stage needs Agave to execute and record an atomic bundle |
//! | [`BankingWorkerPoolFactory`] | none | a fork owns extra or replacement banking workers |
//!
//! A Jito-shaped reference integration lives in the sibling
//! `tpu-extension-api-jito-solana` crate.
//!
//! # Hot Path Contract
//!
//! Vanilla Agave must not pay for hooks it did not enable.
//!
//! [`BankingHooks`] is generic over `F: AccountFilter`, `G: SchedulerGate`, and
//! `L: ExternalLocks`. The no-op defaults (`NoFilter`, `NoGate`, `NoExternalLocks`)
//! implement the hot-path guards as `#[inline(always)]` returning `false`, so the
//! compiler can eliminate the vanilla-path branches.
//!
//! | Core call site | Vanilla behavior |
//! |----------------|------------------|
//! | `scheduler_gate().should_yield()` | inlined `false`; branch eliminated |
//! | `packet_filter().is_active()` guards `packet_filter().is_blocked()` | false for [`NoFilter`]; active filters use static dispatch |
//! | `external_locks().is_active()` guards the lock scan | false for [`NoExternalLocks`]; active locks use static dispatch |
//! | `tip_processor.process(...)` | called on leader-slot transitions only |
//! | `commit_mode()` | copied into `Consumer`; no per-batch policy vtable |
//! | `TpuStage::abort` and `TpuStage::join` | lifecycle only, during shutdown |
//! | [`BundleExecution::execute_and_record_bundle`] | extension stage only; not called by normal packet scheduling |
//!
//! # Current Scope
//!
//! This API intentionally covers lifecycle, launch-context stage factories,
//! packet ingress, optional raw-packet routing, scheduler gating, account
//! filtering, extension account locks, tip setup, batch commit mode, bundle
//! execution/entry recording, and banking worker-pool factories.
//!
//! [`PacketIngress`] sends unverified packets into Agave's normal sigverify path.
//! When [`TpuExtensionsBuilder::with_packet_receiver`] is enabled,
//! [`PacketIngress::packet_receiver`] lets a fork-owned fetch manager receive
//! raw TPU packets before sigverify.
//! [`TrustedBankingIngress`] is available for fork-owned paths that already
//! verified packets and intentionally bypass sigverify.
//!
//! [`BundleExecution`] is the narrow bridge for extension-owned bundle stages
//! that need Agave's runtime to execute, record, and commit an atomic batch.
//! [`BundleEntryPolicy::SplitConflicts`] makes Jito's ordered bundle-entry
//! partitioning requirement explicit without exposing PoH internals to the
//! extension crate.
//!
//! # Jito-Solana Coverage
//!
//! The table below maps every Jito-Solana component to the API extension point
//! it uses. All components are demonstrated in `tpu-extension-api-jito-solana`.
//!
//! | Jito-Solana component | API extension point | Reference file |
//! |-----------------------|---------------------|----------------|
//! | `FetchStageManager` — routes raw QUIC packets before sigverify | [`TpuStageFactory`] · [`TpuStageContext::take_packet_receiver`] · [`PacketIngress::send_to_sigverify`] | `stages/fetch_manager.rs` |
//! | `RelayerStage` — receives packets from a trusted relayer gRPC stream | [`TpuStageFactory`] · [`PacketIngress::send_to_sigverify`] | `stages/relayer.rs` |
//! | `BlockEngineStage` — receives bundles and packets from the block-engine gRPC stream | [`TpuStageFactory`] · [`PacketIngress::send_to_sigverify`] · [`PacketIngress::trusted_banking`] | `stages/block_engine.rs` |
//! | `BundleSigverifyStage` — batch-verifies bundle Ed25519 signatures | [`TpuStage`] (lifecycle only; owns its own inter-stage channels) | `stages/sigverify.rs` |
//! | `BundleStage` — executes verified bundles atomically | [`TpuStageFactory`] · [`BundleExecution::execute_and_record_bundle`] · [`BundleExecutionRequest`] | `stages/bundle.rs` |
//! | `BamManager` — coordinator thread for the bundle-aware mempool | [`TpuStage`] (lifecycle; workers spawned via [`BankingWorkerPoolFactory`]) | `stages/bam_manager.rs` |
//! | `BundleSchedulerGate` — yields the packet scheduler during bundle execution | [`SchedulerGate::should_yield`] (hot path, `#[inline(always)]`) | `hooks/scheduler_gate.rs` |
//! | `BundleExternalLocks` — read/write account locks for in-flight bundles | [`ExternalLocks`] · [`BundleAccountLockView`] (hot path, `#[inline(always)]`) | `hooks/external_locks.rs` |
//! | `TipAccountFilter` — blocks transactions touching Jito tip accounts | [`AccountFilter::is_blocked`] (per-packet hot path) | `hooks/tip_account_filter.rs` |
//! | `TipManager` — handles tip distribution on leader-slot transitions | [`TipProcessor::process`] (called once per leader slot) | `hooks/tip.rs` |
//! | `BamWorkerPoolFactory` — spawns BAM banking worker threads | [`BankingWorkerPoolFactory::spawn_threads`] · [`BankingSchedulerMode::ReplaceInternal`] | `hooks/bam.rs` |
//!
//! `CoreBundleExecutionStub` in `core/src/tpu.rs` is the only intentional stub:
//! a production `BundleExecution` holds `Arc<RwLock<BankForks>>` and
//! `Arc<Mutex<PohRecorder>>` and delegates to the same bank/PoH machinery as
//! Jito-Solana's `BundleConsumer`. The API bridge is correct; the implementation
//! is a follow-up scoped to `agave-core`.

mod extension;
mod traits;

pub use {
    extension::{
        BankingConfig, BankingHooks, BankingHooksBuilder, BankingWorkerPoolFactories,
        PacketIntakeMode, TpuExtensionParts, TpuExtensions, TpuExtensionsBuilder, TpuStageSpec,
    },
    traits::{
        AccountAccess, AccountFilter, BankingSchedulerMode, BankingStageContext,
        BankingWorkerPoolFactory, BatchCommitMode, BatchCommitPolicy, BundleAccountLockView,
        BundleEntryPolicy, BundleExecution, BundleExecutionRequest, BundleExecutionStatus,
        ExternalLocks, LifecycleStage, NoBundleExecution, PacketBatch, PacketIngress, ReadLockView,
        SchedulerGate, TipContext, TipProcessor, TpuStage, TpuStageContext, TpuStageFactory,
        TrustedBankingIngress, TrustedBankingPacketSink,
    },
};
use {solana_pubkey::Pubkey, std::collections::HashSet};

/// No-op [`SchedulerGate`]: the scheduler never yields. Inlines to `false`.
pub struct NoGate;
/// No-op [`AccountFilter`]: no packet is ever blocked. Inlines to `false`.
pub struct NoFilter;
/// No-op [`ExternalLocks`]: no account is ever write-locked. Inlines to `false`.
pub struct NoExternalLocks;
/// No-op [`TipProcessor`]: leader-slot transitions are a no-op.
pub struct NoTip;
/// No-op [`BatchCommitPolicy`]: uses [`BatchCommitMode::standard`].
pub struct StandardCommit;

impl SchedulerGate for NoGate {
    #[inline(always)]
    fn should_yield(&self) -> bool {
        false
    }
}

impl AccountFilter for NoFilter {
    #[inline(always)]
    fn is_blocked(&self, _: &Pubkey) -> bool {
        false
    }
    #[inline(always)]
    fn is_active(&self) -> bool {
        false
    }
}

impl ExternalLocks for NoExternalLocks {
    #[inline(always)]
    fn is_write_locked(&self, _: &Pubkey) -> bool {
        false
    }
    #[inline(always)]
    fn is_read_locked(&self, _: &Pubkey) -> bool {
        false
    }
    #[inline(always)]
    fn is_active(&self) -> bool {
        false
    }
}

impl ReadLockView for NoExternalLocks {
    #[inline(always)]
    fn is_read_locked(&self, _: &Pubkey) -> bool {
        false
    }
    #[inline(always)]
    fn is_active(&self) -> bool {
        false
    }
}

impl BundleAccountLockView for NoExternalLocks {}

impl TipProcessor for NoTip {
    #[inline(always)]
    fn process(&self, _: &TipContext) {}
}

impl BatchCommitPolicy for StandardCommit {
    #[inline(always)]
    fn mode(&self) -> BatchCommitMode {
        BatchCommitMode::standard()
    }
}

/// Bridges `ValidatorConfig::filter_keys` (the `--filter-keys` CLI flag) to [`AccountFilter`].
pub struct SetAccountFilter(pub HashSet<Pubkey>);

impl From<HashSet<Pubkey>> for SetAccountFilter {
    fn from(set: HashSet<Pubkey>) -> Self {
        Self(set)
    }
}

impl AccountFilter for SetAccountFilter {
    #[inline(always)]
    fn is_blocked(&self, pubkey: &Pubkey) -> bool {
        self.0.contains(pubkey)
    }
    #[inline(always)]
    fn is_active(&self) -> bool {
        !self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_pubkey::Pubkey,
        std::{collections::HashSet, sync::Arc},
    };

    #[test]
    fn null_impls_are_no_ops() {
        let key = Pubkey::default();
        assert!(!NoGate.should_yield());
        assert!(!NoFilter.is_blocked(&key));
        assert!(!NoExternalLocks.is_write_locked(&key));
        assert!(!ExternalLocks::is_read_locked(&NoExternalLocks, &key));
        assert!(!NoExternalLocks.conflicts(&key, AccountAccess::Read));
        assert!(!NoExternalLocks.conflicts(&key, AccountAccess::Write));
        assert!(!ExternalLocks::is_active(&NoExternalLocks));
        assert!(!ReadLockView::is_active(&NoExternalLocks));
        assert!(!StandardCommit.mode().reverts_on_error());
        NoTip.process(&TipContext::new(0, 0));
    }

    #[test]
    fn default_extensions_are_empty_and_no_op() {
        let extensions = TpuExtensions::noop();
        assert!(extensions.stages.is_empty());
        assert_eq!(
            extensions.banking.commit_mode(),
            BatchCommitMode::standard()
        );
        assert!(!extensions.banking.scheduler_gate().should_yield());
        assert!(
            !extensions
                .banking
                .packet_filter()
                .is_blocked(&Pubkey::default())
        );
        assert!(
            !extensions
                .banking
                .external_locks()
                .is_write_locked(&Pubkey::default())
        );
    }

    #[test]
    fn banking_hooks_clone_shares_arcs() {
        let hooks = BankingHooks::builder()
            .scheduler_gate(NoGate)
            .packet_filter(NoFilter)
            .external_locks(NoExternalLocks)
            .tip_processor(NoTip)
            .config(BankingConfig::default())
            .build();
        let cloned = hooks.clone();
        assert!(Arc::ptr_eq(&hooks.scheduler_gate, &cloned.scheduler_gate));
        assert!(Arc::ptr_eq(&hooks.packet_filter, &cloned.packet_filter));
        assert!(Arc::ptr_eq(&hooks.external_locks, &cloned.external_locks));
        assert!(Arc::ptr_eq(&hooks.tip_processor, &cloned.tip_processor));
        assert_eq!(hooks.commit_mode(), cloned.commit_mode());
    }

    #[test]
    fn extensions_split_into_stages_and_hooks() {
        let extensions = TpuExtensions::builder().build();
        let parts = extensions.into_parts();
        assert!(parts.stages.is_empty());
        assert!(parts.banking_worker_pools.is_empty());
        assert!(!parts.banking.scheduler_gate().should_yield());
        assert_eq!(parts.packet_intake_mode, PacketIntakeMode::Direct);
    }

    #[test]
    fn set_account_filter_delegates_to_hash_set() {
        let key = Pubkey::from([1u8; 32]);
        let filter = SetAccountFilter(HashSet::from([key]));
        assert!(filter.is_blocked(&key));
        assert!(!filter.is_blocked(&Pubkey::default()));
        assert!(filter.is_active());
        assert!(!SetAccountFilter(HashSet::new()).is_active());
    }
}
