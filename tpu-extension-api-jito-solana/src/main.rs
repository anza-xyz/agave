//! Jito-Solana reference binary.
//!
//! Shows the validator handoff shape: extension stages are owned by the TPU,
//! and banking hooks are owned by BankingStage.

mod hooks;
mod stages;

use {
    agave_tpu_extension_api::{
        BankingConfig, BankingHooks, BatchCommitPolicy, TipConfig, TpuExtensions,
    },
    hooks::{
        BundleCommitPolicy, BundleExternalLocks, BundleSchedulerGate, TipAccountFilter, TipManager,
        tip_account_pubkeys,
    },
    stages::{BlockEngineConfig, BlockEngineStage, BundleSigverifyStage, BundleStage},
    std::sync::{Arc, atomic::AtomicBool, mpsc},
};

fn main() {
    let yield_flag = Arc::new(AtomicBool::new(false));
    let locks = Arc::new(BundleExternalLocks::new());
    let tip_manager = Arc::new(TipManager::new(tip_account_pubkeys()));
    let commit_policy = BundleCommitPolicy;

    let (unverified_tx, unverified_rx) = mpsc::sync_channel(1_024);
    let (verified_tx, verified_rx) = mpsc::sync_channel(1_024);

    let block_engine = BlockEngineStage::spawn(
        BlockEngineConfig {
            block_engine_url: "https://mainnet.block-engine.jito.wtf".to_string(),
            trust_packets: false,
        },
        unverified_tx,
    );
    let sigverify = BundleSigverifyStage::spawn(unverified_rx, verified_tx);
    let bundle_stage = BundleStage::spawn(verified_rx, Arc::clone(&yield_flag), Arc::clone(&locks));

    let hooks = BankingHooks::builder()
        .scheduler_gate(BundleSchedulerGate::new(Arc::clone(&yield_flag)))
        .packet_filter(TipAccountFilter::jito_mainnet())
        .shared_external_locks(Arc::clone(&locks))
        .shared_tip_processor(Arc::clone(&tip_manager))
        .config(BankingConfig {
            tip_config: TipConfig::default(),
            commit_mode: commit_policy.mode(),
        })
        .build();

    let extensions = TpuExtensions::builder()
        .processing_stage(bundle_stage)
        .intake_stage(sigverify)
        .intake_stage(block_engine)
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
