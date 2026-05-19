//! Jito-Solana reference binary.
//!
//! Shows the validator handoff shape: extension stages are owned by the TPU,
//! and banking hooks are owned by BankingStage.

mod hooks;
mod stages;

use {
    agave_tpu_extension_api::{BankingHooks, TpuExtensions, TpuStageSpec},
    hooks::{
        BamWorkerPoolFactory, BundleExternalLocks, BundleSchedulerGate, TipAccountFilter,
        TipManager, tip_account_pubkeys,
    },
    stages::{
        BamManager, BlockEngineConfig, BlockEngineStageFactory, BundleSigverifyStage,
        BundleStageFactory, FetchStageManagerFactory, RelayerStageFactory,
    },
    std::sync::{Arc, atomic::AtomicBool, mpsc},
};

fn main() {
    let yield_flag = Arc::new(AtomicBool::new(false));
    let locks = Arc::new(BundleExternalLocks::new());
    let tip_manager = Arc::new(TipManager::new(tip_account_pubkeys()));
    let _block_builder_fee_info = tip_manager.block_builder_fee_info();

    let (unverified_tx, unverified_rx) = mpsc::sync_channel(1_024);
    let (verified_tx, verified_rx) = mpsc::sync_channel(1_024);

    let block_engine = BlockEngineStageFactory::new(
        BlockEngineConfig {
            block_engine_url: "https://mainnet.block-engine.jito.wtf".to_string(),
            trust_packets: false,
        },
        unverified_tx,
    );
    let sigverify = BundleSigverifyStage::spawn(unverified_rx, verified_tx);
    let bundle_stage =
        BundleStageFactory::new(verified_rx, Arc::clone(&yield_flag), Arc::clone(&locks));
    let bam_manager = BamManager::spawn();

    let hooks = BankingHooks::builder()
        .scheduler_gate(BundleSchedulerGate::new(Arc::clone(&yield_flag)))
        .packet_filter(TipAccountFilter::jito_mainnet())
        .shared_external_locks(Arc::clone(&locks))
        .shared_tip_processor(Arc::clone(&tip_manager))
        .build();

    let extensions = TpuExtensions::builder()
        .with_packet_receiver()
        .processing_stage(TpuStageSpec::factory(bundle_stage))
        .processing_stage(TpuStageSpec::running(bam_manager))
        .intake_stage(TpuStageSpec::running(sigverify))
        .intake_stage(TpuStageSpec::factory(FetchStageManagerFactory))
        .intake_stage(TpuStageSpec::factory(RelayerStageFactory))
        .intake_stage(TpuStageSpec::factory(block_engine))
        .banking_worker_pool(BamWorkerPoolFactory::default().replacing_internal_scheduler())
        .banking_hooks(hooks)
        .build();

    // let validator = Validator::new_with_exit_with_tpu_extensions::<
    //     TipAccountFilter,
    //     BundleSchedulerGate,
    //     BundleExternalLocks,
    // >(
    //     node,
    //     identity_keypair,
    //     ledger_path,
    //     vote_account,
    //     authorized_voter_keypairs,
    //     cluster_entrypoints,
    //     &validator_config,
    //     rpc_to_plugin_manager_receiver,
    //     start_progress,
    //     socket_addr_space,
    //     tpu_config,
    //     admin_rpc_service_post_init,
    //     xdp_builder_with_src_addr,
    //     extensions,
    //     exit,
    // )
    // .expect("validator startup failed");
    //
    // validator
    //     .listen_for_signals()
    //     .expect("validator signal listener failed");
    // validator.join();

    let _ = extensions;
}

/// Integration test: proves the full Jito-Solana pipeline spawns, runs, and
/// shuts down cleanly via the TPU extension API lifecycle protocol.
///
/// Uses a [`MockTpuStageContext`] to stand in for what Agave's `Tpu` would
/// provide at startup. Covers every stage in the pipeline and verifies that
/// `abort` + `join` in reverse-push order completes without deadlock.
#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_tpu_extension_api::{
            AccountFilter, BundleExecution, BundleExecutionRequest, BundleExecutionStatus,
            ExternalLocks, PacketBatch, PacketIngress, SchedulerGate, TpuStage, TpuStageContext,
        },
        std::sync::{Mutex, atomic::Ordering},
    };

    struct MockTpuStageContext {
        packet_ingress: PacketIngress,
        packet_receiver: Arc<Mutex<Option<crossbeam_channel::Receiver<PacketBatch>>>>,
        bundle_execution: Arc<dyn BundleExecution>,
        exit: Arc<AtomicBool>,
    }

    impl MockTpuStageContext {
        fn new(exit: Arc<AtomicBool>) -> Self {
            let (sigverify_tx, _sigverify_rx) = crossbeam_channel::unbounded();
            let (_raw_tx, raw_rx) = crossbeam_channel::unbounded();
            Self {
                packet_ingress: PacketIngress::new(sigverify_tx),
                packet_receiver: Arc::new(Mutex::new(Some(raw_rx))),
                bundle_execution: Arc::new(NoopBundleExecution),
                exit,
            }
        }
    }

    impl TpuStageContext for MockTpuStageContext {
        fn packet_ingress(&self) -> &PacketIngress {
            &self.packet_ingress
        }
        fn take_packet_receiver(&self) -> Option<crossbeam_channel::Receiver<PacketBatch>> {
            self.packet_receiver.lock().unwrap().take()
        }
        fn exit(&self) -> Arc<AtomicBool> {
            Arc::clone(&self.exit)
        }
        fn bundle_execution(&self) -> Arc<dyn BundleExecution> {
            Arc::clone(&self.bundle_execution)
        }
    }

    struct NoopBundleExecution;
    impl BundleExecution for NoopBundleExecution {
        fn execute_and_record_bundle(&self, _: BundleExecutionRequest) -> BundleExecutionStatus {
            BundleExecutionStatus::Unavailable
        }
    }

    #[test]
    fn jito_pipeline_spawns_aborts_and_joins_cleanly() {
        let exit = Arc::new(AtomicBool::new(false));
        let yield_flag = Arc::new(AtomicBool::new(false));
        let locks = Arc::new(BundleExternalLocks::new());
        let tip_manager = Arc::new(TipManager::new(tip_account_pubkeys()));

        let (unverified_tx, unverified_rx) = mpsc::sync_channel(1_024);
        let (verified_tx, verified_rx) = mpsc::sync_channel(1_024);

        let block_engine = BlockEngineStageFactory::new(
            BlockEngineConfig {
                block_engine_url: "https://mainnet.block-engine.jito.wtf".to_string(),
                trust_packets: false,
            },
            unverified_tx,
        );
        let sigverify = BundleSigverifyStage::spawn(unverified_rx, verified_tx);
        let bundle_stage =
            BundleStageFactory::new(verified_rx, Arc::clone(&yield_flag), Arc::clone(&locks));
        let bam_manager = BamManager::spawn();

        let hooks = BankingHooks::builder()
            .scheduler_gate(BundleSchedulerGate::new(Arc::clone(&yield_flag)))
            .packet_filter(TipAccountFilter::jito_mainnet())
            .shared_external_locks(Arc::clone(&locks))
            .shared_tip_processor(Arc::clone(&tip_manager))
            .build();

        let extensions = TpuExtensions::builder()
            .with_packet_receiver()
            .processing_stage(TpuStageSpec::factory(bundle_stage))
            .processing_stage(TpuStageSpec::running(bam_manager))
            .intake_stage(TpuStageSpec::running(sigverify))
            .intake_stage(TpuStageSpec::factory(FetchStageManagerFactory))
            .intake_stage(TpuStageSpec::factory(RelayerStageFactory))
            .intake_stage(TpuStageSpec::factory(block_engine))
            .banking_worker_pool(BamWorkerPoolFactory::default().replacing_internal_scheduler())
            .banking_hooks(hooks)
            .build();

        // Verify packet routing mode is set correctly.
        assert!(extensions.packet_intake_mode.is_routed());

        let parts = extensions.into_parts();

        // Six stages total: 2 processing (bundle_stage, bam_manager) +
        //                   4 intake    (sigverify, fetch_manager, relayer, block_engine)
        assert_eq!(parts.stages.len(), 6);

        // Hooks wired correctly:
        // - Gate is false (yield_flag not raised yet; would be true during bundle execution)
        // - Filter is active (tip accounts registered)
        // - Locks report inactive (no bundles running; count-based hot-path guard is correct)
        assert!(!parts.banking.scheduler_gate().should_yield());
        assert!(parts.banking.packet_filter().is_active());
        assert!(!parts.banking.external_locks().is_active());

        // One worker pool registered and it replaces the internal scheduler.
        assert_eq!(parts.banking_worker_pools.len(), 1);
        assert!(parts.banking_worker_pools.replaces_internal_scheduler());

        // Spawn all stages (factories get the mock context, running stages pass through).
        let ctx = MockTpuStageContext::new(Arc::clone(&exit));
        let stages: Vec<Box<dyn TpuStage>> =
            parts.stages.into_iter().map(|s| s.spawn(&ctx)).collect();

        // Two-phase shutdown: signal all in reverse order, then join all.
        // This mirrors the LifecycleStage protocol documented on the trait.
        exit.store(true, Ordering::Release);
        for stage in stages.iter().rev() {
            stage.abort();
        }
        for stage in stages.into_iter().rev() {
            stage.join().expect("stage join failed");
        }
    }
}
