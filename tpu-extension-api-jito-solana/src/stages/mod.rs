mod bam_manager;
mod block_engine;
mod bundle;
mod fetch_manager;
mod relayer;
mod sigverify;

use {agave_banking_stage_ingress_types::BankingPacketBatch, solana_pubkey::Pubkey};

pub use bam_manager::BamManager;
pub use block_engine::{BlockEngineConfig, BlockEngineStageFactory};
pub use bundle::BundleStageFactory;
pub use fetch_manager::FetchStageManagerFactory;
pub use relayer::RelayerStageFactory;
pub use sigverify::BundleSigverifyStage;

/// Inter-stage bundle payload (reference stand-in for the production gRPC bundle type).
///
/// Production Jito-Solana uses separate pre- and post-sigverify types; the
/// reference collapses both into one to keep the stub minimal.
pub struct PacketBundle {
    /// Sigverified packet batches that make up the bundle.
    pub packets: BankingPacketBatch,
    /// Read-only accounts locked for the duration of bundle execution.
    pub read_locks: Vec<Pubkey>,
    /// Writable accounts locked for the duration of bundle execution.
    pub write_locks: Vec<Pubkey>,
}
