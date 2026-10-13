use {
    agave_snapshots::snapshot_config::SnapshotConfig,
    solana_accounts_db::utils::{
        create_all_accounts_run_dirs, create_and_canonicalize_directories,
        create_and_canonicalize_directory,
    },
    solana_runtime::snapshot_utils,
    std::{error::Error, path::PathBuf},
};

/// Creates the account and snapshot directories, checks that no account path is the snapshots
/// directory, and removes temporary snapshot archives. Snapshot paths are replaced with canonical
/// ones and account paths with their canonical run/ directories.
pub(super) fn prepare(
    account_paths: &mut Vec<PathBuf>,
    snapshot_config: &mut SnapshotConfig,
) -> Result<(), Box<dyn Error>> {
    let canonical_account_paths = create_and_canonicalize_directories(account_paths.iter())
        .map_err(|err| format!("unable to access account path: {err}"))?;
    let snapshots_dir = snapshot_config
        .bank_snapshots_dir
        .parent()
        .expect("bank snapshots are in the snapshots directory");
    let snapshots_dir = create_and_canonicalize_directory(snapshots_dir).map_err(|err| {
        format!(
            "failed to create snapshots directory '{}': {err}",
            snapshots_dir.display()
        )
    })?;
    if canonical_account_paths.contains(&snapshots_dir) {
        return Err(
            "the --accounts and --snapshots paths must be unique since they both create \
             'snapshots' subdirectories, otherwise there may be collisions"
                .into(),
        );
    }
    for (dir, name) in [
        (&mut snapshot_config.bank_snapshots_dir, "bank snapshots"),
        (
            &mut snapshot_config.full_snapshot_archives_dir,
            "full snapshot archives",
        ),
        (
            &mut snapshot_config.incremental_snapshot_archives_dir,
            "incremental snapshot archives",
        ),
    ] {
        *dir = create_and_canonicalize_directory(&dir).map_err(|err| {
            format!(
                "failed to create {name} directory '{}': {err}",
                dir.display()
            )
        })?;
    }
    *account_paths = create_all_accounts_run_dirs(&canonical_account_paths)
        .map_err(|err| format!("unable to create account directories: {err}"))?;
    snapshot_utils::remove_tmp_snapshot_archives(&snapshot_config.full_snapshot_archives_dir);
    snapshot_utils::remove_tmp_snapshot_archives(
        &snapshot_config.incremental_snapshot_archives_dir,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_snapshots::paths::BANK_SNAPSHOTS_DIR,
        solana_accounts_db::utils::ACCOUNTS_RUN_DIR,
        std::{fs, path::Path},
    };

    fn build_snapshot_config(snapshots_dir: &Path) -> SnapshotConfig {
        SnapshotConfig {
            bank_snapshots_dir: snapshots_dir.join(BANK_SNAPSHOTS_DIR),
            full_snapshot_archives_dir: snapshots_dir.to_path_buf(),
            incremental_snapshot_archives_dir: snapshots_dir.join("incremental"),
            ..SnapshotConfig::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_prepare() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp_path = fs::canonicalize(tmp_dir.path()).unwrap();
        let ledger_path = tmp_path.join("ledger");
        fs::create_dir(&ledger_path).unwrap();
        let link_path = tmp_path.join("link");
        std::os::unix::fs::symlink(&ledger_path, &link_path).unwrap();

        let mut account_paths = vec![link_path.join("accounts")];
        let mut snapshot_config = build_snapshot_config(&link_path);
        prepare(&mut account_paths, &mut snapshot_config).unwrap();
        let account_run_path = ledger_path.join("accounts").join(ACCOUNTS_RUN_DIR);
        assert_eq!(account_paths, [account_run_path.as_path()]);
        assert!(account_run_path.is_dir());
        let bank_snapshots_dir = ledger_path.join(BANK_SNAPSHOTS_DIR);
        assert_eq!(snapshot_config.bank_snapshots_dir, bank_snapshots_dir);
        assert!(bank_snapshots_dir.is_dir());
        assert_eq!(snapshot_config.full_snapshot_archives_dir, ledger_path);
        let incremental_snapshot_archives_dir = ledger_path.join("incremental");
        assert_eq!(
            snapshot_config.incremental_snapshot_archives_dir,
            incremental_snapshot_archives_dir
        );
        assert!(incremental_snapshot_archives_dir.is_dir());

        let error = prepare(
            &mut vec![link_path.clone()],
            &mut build_snapshot_config(&ledger_path),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("must be unique"), "{error}");
    }
}
