#![cfg(feature = "agave-unstable-api")]
use {
    agave_snapshots::unpack_genesis_archive,
    log::*,
    solana_download_utils::download_genesis_if_missing,
    solana_genesis_config::{DEFAULT_GENESIS_ARCHIVE, GenesisConfig},
    solana_hash::Hash,
    solana_rpc_client::rpc_client::RpcClient,
    std::net::SocketAddr,
};

mod open;

/// An error while fetching the genesis config, distinguishing whether the
/// failure can be resolved by retrying.
#[derive(Debug, thiserror::Error)]
pub enum GenesisFetchError {
    /// The local genesis could not be loaded, or its hash does not match the
    /// expected genesis hash, and no download can resolve it. This is fatal:
    /// the operator must fix the local genesis, or adjust the
    /// `--expected-genesis-hash` / `--no-genesis-fetch` arguments.
    #[error("{0}")]
    Local(String),

    /// The failure may be resolved by retrying, possibly with a different RPC
    /// node: e.g. the genesis downloaded from the node did not match the
    /// expected genesis hash, or the download itself failed.
    #[error("{0}")]
    Downloaded(String),
}

/// An error loading the local genesis config.
#[derive(Debug, thiserror::Error)]
enum LocalGenesisError {
    /// The genesis.bin file is missing or unreadable.
    #[error("{0}")]
    Load(String),

    /// The genesis hash does not match the expected genesis hash.
    #[error("{0}")]
    HashMismatch(String),
}

/// Maps a local genesis failure to its final classification.
///
/// `download_attempted` decides whether a load failure is fatal: if no
/// download will be attempted, retrying cannot resolve it. A hash mismatch is
/// always fatal: the operator must resolve it, per
/// <https://github.com/anza-xyz/agave/issues/12900>.
fn classify_local_genesis_error(
    err: LocalGenesisError,
    download_attempted: bool,
) -> GenesisFetchError {
    match err {
        LocalGenesisError::HashMismatch(err) => GenesisFetchError::Local(err),
        LocalGenesisError::Load(err) if !download_attempted => GenesisFetchError::Local(err),
        LocalGenesisError::Load(err) => GenesisFetchError::Downloaded(err),
    }
}

fn check_genesis_hash(
    genesis_config: &GenesisConfig,
    expected_genesis_hash: Option<Hash>,
    source: &str,
) -> Result<(), String> {
    let genesis_hash = genesis_config.hash();

    if let Some(expected_genesis_hash) = expected_genesis_hash
        && expected_genesis_hash != genesis_hash
    {
        return Err(format!(
            "Genesis hash mismatch: expected {expected_genesis_hash} but {source} genesis hash \
             is {genesis_hash}",
        ));
    }

    Ok(())
}

fn load_local_genesis(
    ledger_path: &std::path::Path,
    expected_genesis_hash: Option<Hash>,
) -> Result<GenesisConfig, LocalGenesisError> {
    let existing_genesis = GenesisConfig::load(ledger_path)
        .map_err(|err| LocalGenesisError::Load(format!("Failed to load genesis config: {err}")))?;
    check_genesis_hash(&existing_genesis, expected_genesis_hash, "local")
        .map_err(LocalGenesisError::HashMismatch)?;

    Ok(existing_genesis)
}

fn get_genesis_config(
    rpc_addr: &SocketAddr,
    ledger_path: &std::path::Path,
    expected_genesis_hash: Option<Hash>,
    max_genesis_archive_unpacked_size: u64,
    no_genesis_fetch: bool,
    use_progress_bar: bool,
) -> Result<GenesisConfig, GenesisFetchError> {
    if no_genesis_fetch {
        return load_local_genesis(ledger_path, expected_genesis_hash)
            .map_err(|err| classify_local_genesis_error(err, /* download_attempted */ false));
    }

    let genesis_package = ledger_path.join(DEFAULT_GENESIS_ARCHIVE);
    if let Ok(tmp_genesis_package) =
        download_genesis_if_missing(rpc_addr, &genesis_package, use_progress_bar)
    {
        unpack_genesis_archive(
            &tmp_genesis_package,
            ledger_path,
            max_genesis_archive_unpacked_size,
        )
        .map_err(|err| {
            GenesisFetchError::Downloaded(format!(
                "Failed to unpack downloaded genesis config: {err}"
            ))
        })?;

        let downloaded_genesis = GenesisConfig::load(ledger_path).map_err(|err| {
            GenesisFetchError::Downloaded(format!(
                "Failed to load downloaded genesis config: {err}"
            ))
        })?;

        check_genesis_hash(&downloaded_genesis, expected_genesis_hash, "downloaded")
            .map_err(GenesisFetchError::Downloaded)?;

        std::fs::rename(tmp_genesis_package, genesis_package)
            .map_err(|err| GenesisFetchError::Local(format!("Unable to rename: {err:?}")))?;

        Ok(downloaded_genesis)
    } else if genesis_package.exists() {
        // Download skipped because the genesis archive already exists locally:
        // no download can resolve a local genesis failure.
        load_local_genesis(ledger_path, expected_genesis_hash)
            .map_err(|err| classify_local_genesis_error(err, /* download_attempted */ false))
    } else {
        // The download failed: retrying may resolve it, but a local genesis
        // hash mismatch is still fatal.
        load_local_genesis(ledger_path, expected_genesis_hash)
            .map_err(|err| classify_local_genesis_error(err, /* download_attempted */ true))
    }
}

fn set_and_verify_expected_genesis_hash(
    genesis_config: GenesisConfig,
    expected_genesis_hash: &mut Option<Hash>,
    rpc_client: &RpcClient,
) -> Result<(), GenesisFetchError> {
    let genesis_hash = genesis_config.hash();
    if expected_genesis_hash.is_none() {
        info!("Expected genesis hash set to {genesis_hash}");
        *expected_genesis_hash = Some(genesis_hash);
    }
    let expected_genesis_hash = expected_genesis_hash.unwrap();

    // Sanity check that the RPC node is using the expected genesis hash before
    // downloading a snapshot from it
    let rpc_genesis_hash = rpc_client.get_genesis_hash().map_err(|err| {
        GenesisFetchError::Downloaded(format!("Failed to get genesis hash: {err}"))
    })?;

    if expected_genesis_hash != rpc_genesis_hash {
        return Err(GenesisFetchError::Downloaded(format!(
            "Genesis hash mismatch: expected {expected_genesis_hash} but RPC node genesis hash is \
             {rpc_genesis_hash}"
        )));
    }

    Ok(())
}

pub fn download_then_check_genesis_hash(
    rpc_addr: &SocketAddr,
    ledger_path: &std::path::Path,
    expected_genesis_hash: &mut Option<Hash>,
    max_genesis_archive_unpacked_size: u64,
    no_genesis_fetch: bool,
    use_progress_bar: bool,
    rpc_client: &RpcClient,
) -> Result<(), GenesisFetchError> {
    let genesis_config = get_genesis_config(
        rpc_addr,
        ledger_path,
        *expected_genesis_hash,
        max_genesis_archive_unpacked_size,
        no_genesis_fetch,
        use_progress_bar,
    )?;

    set_and_verify_expected_genesis_hash(genesis_config, expected_genesis_hash, rpc_client)
}

pub use open::{MAX_GENESIS_ARCHIVE_UNPACKED_SIZE, OpenGenesisConfigError, open_genesis_config};

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_genesis_config::DEFAULT_GENESIS_ARCHIVE,
        std::{fs, net::SocketAddr, path::PathBuf, process},
    };

    // An RpcClient is lazy: with `no_genesis_fetch`, or when the genesis
    // archive already exists locally, it is never contacted, so these tests
    // run without a live RPC node.
    fn rpc_addr() -> SocketAddr {
        "127.0.0.1:1".parse().unwrap()
    }

    fn tmp_ledger_path(name: &str) -> PathBuf {
        let ledger_path = std::env::temp_dir().join(format!(
            "genesis-utils-{name}-{}-{}",
            process::id(),
            Hash::new_unique()
        ));
        let _ = fs::remove_dir_all(&ledger_path);
        fs::create_dir_all(&ledger_path).unwrap();
        ledger_path
    }

    #[test]
    fn test_no_genesis_fetch_with_missing_genesis_is_local_error() {
        let ledger_path = tmp_ledger_path("missing");
        let err = get_genesis_config(
            &rpc_addr(),
            &ledger_path,
            None,
            MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            /* no_genesis_fetch */ true,
            /* use_progress_bar */ false,
        )
        .unwrap_err();

        assert!(matches!(err, GenesisFetchError::Local(_)));
        assert!(err.to_string().contains("Failed to load genesis config"));
    }

    #[test]
    fn test_local_genesis_hash_mismatch_is_local_error() {
        let ledger_path = tmp_ledger_path("mismatch");
        // A local genesis.bin whose hash cannot match the expected hash, plus a
        // genesis archive so download_genesis_if_missing skips downloading. No
        // download occurred, so the mismatch is a local failure (issue #12900).
        GenesisConfig::new(&[], &[]).write(&ledger_path).unwrap();
        fs::write(
            ledger_path.join(DEFAULT_GENESIS_ARCHIVE),
            b"genesis archive",
        )
        .unwrap();

        let err = get_genesis_config(
            &rpc_addr(),
            &ledger_path,
            Some(Hash::new_unique()),
            MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            /* no_genesis_fetch */ false,
            /* use_progress_bar */ false,
        )
        .unwrap_err();

        assert!(matches!(err, GenesisFetchError::Local(_)));
        let msg = err.to_string();
        assert!(msg.contains("Genesis hash mismatch"));
        assert!(msg.contains("local genesis hash"));
        assert!(!msg.contains("downloaded genesis hash"));
    }

    #[test]
    fn test_local_genesis_hash_match_succeeds() {
        let ledger_path = tmp_ledger_path("match");
        let genesis_config = GenesisConfig::new(&[], &[]);
        genesis_config.write(&ledger_path).unwrap();
        fs::write(
            ledger_path.join(DEFAULT_GENESIS_ARCHIVE),
            b"genesis archive",
        )
        .unwrap();

        let loaded = get_genesis_config(
            &rpc_addr(),
            &ledger_path,
            Some(genesis_config.hash()),
            MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            /* no_genesis_fetch */ false,
            /* use_progress_bar */ false,
        )
        .unwrap();

        assert_eq!(loaded.hash(), genesis_config.hash());
    }

    #[test]
    fn test_download_failure_without_local_genesis_is_downloaded_error() {
        let ledger_path = tmp_ledger_path("download-failure");
        // The download fails (connection refused) and no local genesis exists:
        // retrying may resolve it, so this is not fatal. The old code
        // blacklisted the RPC node and looped forever; this classification
        // must not exit(1) on a transient download failure.
        let err = get_genesis_config(
            &rpc_addr(),
            &ledger_path,
            None,
            MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            /* no_genesis_fetch */ false,
            /* use_progress_bar */ false,
        )
        .unwrap_err();

        assert!(matches!(err, GenesisFetchError::Downloaded(_)));
        assert!(err.to_string().contains("Failed to load genesis config"));
    }

    #[test]
    fn test_download_failure_with_mismatched_local_genesis_is_local_error() {
        let ledger_path = tmp_ledger_path("download-failure-mismatch");
        // The download fails (connection refused) but a local genesis exists
        // whose hash cannot match the expected hash: the operator must resolve
        // the mismatch, so this is fatal (issue #12900).
        GenesisConfig::new(&[], &[]).write(&ledger_path).unwrap();

        let err = get_genesis_config(
            &rpc_addr(),
            &ledger_path,
            Some(Hash::new_unique()),
            MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            /* no_genesis_fetch */ false,
            /* use_progress_bar */ false,
        )
        .unwrap_err();

        assert!(matches!(err, GenesisFetchError::Local(_)));
        let msg = err.to_string();
        assert!(msg.contains("Genesis hash mismatch"));
        assert!(msg.contains("local genesis hash"));
    }
}
