mod block_engine;
mod bundle;
mod sigverify;

use solana_pubkey::Pubkey;

pub use block_engine::{BlockEngineConfig, BlockEngineStage};
pub use bundle::BundleStage;
pub use sigverify::BundleSigverifyStage;

// Reference-only bundle payload. Production Jito-Solana would use its
// block-engine bundle type.
pub(super) struct PacketBundle {
    pub packets: Vec<Vec<u8>>,
    pub write_locks: Vec<Pubkey>,
}
