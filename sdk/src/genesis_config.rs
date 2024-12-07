#![cfg(feature = "full")]
#[deprecated(
    since = "2.2.0",
    note = "Use `solana_cluster_type::ClusterType` instead."
)]
pub use solana_cluster_type::ClusterType;
#[deprecated(since = "2.2.0", note = "Use `solana-genesis-config` crate instead.")]
pub use solana_genesis_config::{
    create_genesis_config, GenesisConfig, DEFAULT_GENESIS_ARCHIVE, DEFAULT_GENESIS_DOWNLOAD_PATH,
    DEFAULT_GENESIS_FILE, UNUSED_DEFAULT,
};
