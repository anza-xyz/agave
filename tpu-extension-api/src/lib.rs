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
//! `Validator::new_with_exit(...)`. That path builds default [`TpuExtensions`]
//! internally and still bridges `ValidatorConfig::filter_keys` into
//! [`SetAccountFilter`].
//!
//! ```ignore
//! let validator = Validator::new_with_exit(
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
//!     exit,
//! )?;
//! ```
//!
//! # Fork Integration
//!
//! Build a [`BankingHooks`] value from the extension points the fork needs, then
//! pass it with any owned extension stages through [`TpuExtensions`]:
//!
//! ```ignore
//! let hooks = BankingHooks::builder()
//!     .scheduler_gate(MyGate)
//!     .packet_filter(MyFilter)
//!     .shared_external_locks(Arc::clone(&my_locks))
//!     .shared_tip_processor(Arc::clone(&my_tip_processor))
//!     .config(BankingConfig {
//!         tip_config: TipConfig { validator_fee_payer },
//!         commit_mode: BatchCommitMode::all_or_nothing(),
//!     })
//!     .build();
//!
//! let extensions = TpuExtensions::builder()
//!     .processing_stage(my_executor_stage)
//!     .intake_stage(my_ingress_stage)
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
//!
//! # Current Scope
//!
//! This API intentionally covers lifecycle, scheduler gating, account filtering,
//! extension account locks, tip setup, and batch commit mode.
//!
//! Packet injection is intentionally deferred. Injecting pre-verified packets
//! directly into banking, bypassing sigverify, needs a separate ingress contract.
//! [`ReadLockView`] and [`BundleAccountLockView`] are reserved for that follow-up.
//!
//! Entry partitioning is also intentionally not an Agave hook. Forks that need
//! write-conflicting bundle recording should keep that algorithm with their
//! bundle executor.
//!
//! # Known Limitation
//!
//! `ValidatorConfig::filter_keys` is not applied by
//! `Validator::new_with_exit_with_tpu_extensions`.
//!
//! The standard `Validator::new_with_exit` path applies it by building a
//! [`SetAccountFilter`]. Fork constructors that bypass that path must include
//! equivalent behavior in their own [`AccountFilter`] if they need it.

mod defaults;
mod extension;
mod traits;

pub use defaults::{NoExternalLocks, NoFilter, NoGate, NoTip, SetAccountFilter, StandardCommit};
pub use extension::{
    BankingConfig, BankingHandles, BankingHooks, BankingHooksBuilder, TpuExtensions,
    TpuExtensionsBuilder,
};
pub use traits::{
    AccountFilter, BatchCommitMode, BatchCommitPolicy, BundleAccountLockView, ExternalLocks,
    LifecycleStage, ReadLockView, SchedulerGate, TipContext, TipProcessor, TpuStage,
};

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
        assert!(!NoExternalLocks.is_read_locked(&key));
        assert!(!ExternalLocks::is_active(&NoExternalLocks));
        assert!(!ReadLockView::is_active(&NoExternalLocks));
        assert!(!StandardCommit.mode().reverts_on_error());
        NoTip.process(&TipContext::new(0, 0));
    }

    #[test]
    fn default_extensions_are_empty_and_no_op() {
        let extensions = TpuExtensions::default();
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
    fn extension_banking_handles_expose_composable_trait_objects() {
        let extensions = TpuExtensions::default();
        let handles = extensions.handles();
        assert!(!handles.scheduler_gate.should_yield());
        assert!(!handles.packet_filter.is_blocked(&Pubkey::default()));
        assert!(!handles.external_locks.is_write_locked(&Pubkey::default()));
        handles.tip_processor.process(&TipContext::new(0, 0));
    }

    #[test]
    fn extensions_split_into_stages_and_hooks() {
        let extensions = TpuExtensions::builder().build();
        let (stages, hooks) = extensions.into_parts();
        assert!(stages.is_empty());
        assert!(!hooks.scheduler_gate().should_yield());
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
