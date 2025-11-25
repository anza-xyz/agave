use {
    agave_snapshots::{hardened_unpack::UnpackError, unpack_genesis_archive},
    solana_genesis_config::{GenesisConfig, DEFAULT_GENESIS_ARCHIVE},
    std::path::Path,
    thiserror::Error,
};

pub const MAX_GENESIS_ARCHIVE_UNPACKED_SIZE: u64 = 10 * 1024 * 1024; // 10 MiB

#[derive(Error, Debug)]
pub enum OpenGenesisConfigError {
    #[error("unpack error: {0}")]
    Unpack(#[from] UnpackError),
    #[error("Genesis load error: {0}")]
    Load(#[from] std::io::Error),
}

pub fn open_genesis_config(
    ledger_path: &Path,
    max_genesis_archive_unpacked_size: u64,
) -> Result<GenesisConfig, OpenGenesisConfigError> {
    match GenesisConfig::load(ledger_path) {
        Ok(genesis_config) => Ok(genesis_config),
        Err(load_err) => {
            log::warn!(
                "Failed to load genesis_config at {ledger_path:?}: {load_err}. Will attempt to \
                 unpack genesis archive and then retry loading."
            );

            let genesis_package = ledger_path.join(DEFAULT_GENESIS_ARCHIVE);
            unpack_genesis_archive(
                &genesis_package,
                ledger_path,
                max_genesis_archive_unpacked_size,
            )?;
            GenesisConfig::load(ledger_path).map_err(OpenGenesisConfigError::Load)
        }
    }
}
