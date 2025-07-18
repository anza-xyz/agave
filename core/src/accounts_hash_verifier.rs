//! Service to calculate accounts hashes

use {
    crate::snapshot_packager_service::PendingSnapshotPackages,
    crossbeam_channel::{Receiver, Sender},
    solana_clock::DEFAULT_MS_PER_SLOT,
    solana_measure::measure_us,
    solana_runtime::{
        snapshot_config::SnapshotConfig,
        snapshot_controller::SnapshotController,
        snapshot_package::{
            self, AccountsPackage, AccountsPackageKind, SnapshotKind, SnapshotPackage,
        },
    },
    std::{
        io,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub struct AccountsHashVerifier {
    t_accounts_hash_verifier: JoinHandle<()>,
}

impl AccountsHashVerifier {
    pub fn new(
        accounts_package_sender: Sender<AccountsPackage>,
        accounts_package_receiver: Receiver<AccountsPackage>,
        pending_snapshot_packages: Arc<Mutex<PendingSnapshotPackages>>,
        exit: Arc<AtomicBool>,
        snapshot_controller: Arc<SnapshotController>,
    ) -> Self {
        // If there are no accounts packages to process, limit how often we re-check
        const LOOP_LIMITER: Duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);
        let t_accounts_hash_verifier = Builder::new()
            .name("solAcctHashVer".to_string())
            .spawn(move || {
                info!("AccountsHashVerifier has started");
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    let Some((
                        accounts_package,
                        num_outstanding_accounts_packages,
                        num_re_enqueued_accounts_packages,
                    )) = Self::get_next_accounts_package(
                        &accounts_package_sender,
                        &accounts_package_receiver,
                    )
                    else {
                        std::thread::sleep(LOOP_LIMITER);
                        continue;
                    };
                    info!("handling accounts package: {accounts_package:?}");
                    let enqueued_time = accounts_package.enqueued.elapsed();

                    let snapshot_config = snapshot_controller.snapshot_config();
                    let (result, handling_time_us) = measure_us!(Self::process_accounts_package(
                        accounts_package,
                        &pending_snapshot_packages,
                        snapshot_config,
                    ));
                    if let Err(err) = result {
                        error!(
                            "Stopping AccountsHashVerifier! Fatal error while processing accounts \
                             package: {err}"
                        );
                        exit.store(true, Ordering::Relaxed);
                        break;
                    }

                    datapoint_info!(
                        "accounts_hash_verifier",
                        (
                            "num_outstanding_accounts_packages",
                            num_outstanding_accounts_packages,
                            i64
                        ),
                        (
                            "num_re_enqueued_accounts_packages",
                            num_re_enqueued_accounts_packages,
                            i64
                        ),
                        ("enqueued_time_us", enqueued_time.as_micros(), i64),
                        ("handling_time_us", handling_time_us, i64),
                    );
                }
                info!("AccountsHashVerifier has stopped");
            })
            .unwrap();
        Self {
            t_accounts_hash_verifier,
        }
    }

    /// Get the next accounts package to handle
    ///
    /// Look through the accounts package channel to find the highest priority one to handle next.
    /// If there are no accounts packages in the channel, return None.  Otherwise return the
    /// highest priority one.  Unhandled accounts packages with slots GREATER-THAN the handled one
    /// will be re-enqueued.  The remaining will be dropped.
    ///
    /// Also return the number of accounts packages initially in the channel, and the number of
    /// ones re-enqueued.
    fn get_next_accounts_package(
        accounts_package_sender: &Sender<AccountsPackage>,
        accounts_package_receiver: &Receiver<AccountsPackage>,
    ) -> Option<(
        AccountsPackage,
        /*num outstanding accounts packages*/ usize,
        /*num re-enqueued accounts packages*/ usize,
    )> {
        let mut accounts_packages: Vec<_> = accounts_package_receiver.try_iter().collect();
        let accounts_packages_len = accounts_packages.len();
        debug!("outstanding accounts packages ({accounts_packages_len}): {accounts_packages:?}");

        // NOTE: This code to select the next request is mirrored in AccountsBackgroundService.
        // Please ensure they stay in sync.
        match accounts_packages_len {
            0 => None,
            1 => {
                // SAFETY: We know the len is 1, so `pop` will return `Some`
                let accounts_package = accounts_packages.pop().unwrap();
                Some((accounts_package, 1, 0))
            }
            _ => {
                // Get the two highest priority requests, `y` and `z`.
                // By asking for the second-to-last element to be in its final sorted position, we
                // also ensure that the last element is also sorted.
                // Note, we no longer need the second-to-last element; this code can be refactored.
                let (_, _y, z) = accounts_packages.select_nth_unstable_by(
                    accounts_packages_len - 2,
                    snapshot_package::cmp_accounts_packages_by_priority,
                );
                assert_eq!(z.len(), 1);

                // SAFETY: We know the len is > 1, so `pop` will return `Some`
                let accounts_package = accounts_packages.pop().unwrap();

                let handled_accounts_package_slot = accounts_package.slot;
                // re-enqueue any remaining accounts packages for slots GREATER-THAN the accounts package
                // that will be handled
                let num_re_enqueued_accounts_packages = accounts_packages
                    .into_iter()
                    .filter(|accounts_package| {
                        accounts_package.slot > handled_accounts_package_slot
                    })
                    .map(|accounts_package| {
                        accounts_package_sender
                            .try_send(accounts_package)
                            .expect("re-enqueue accounts package")
                    })
                    .count();

                Some((
                    accounts_package,
                    accounts_packages_len,
                    num_re_enqueued_accounts_packages,
                ))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_accounts_package(
        accounts_package: AccountsPackage,
        pending_snapshot_packages: &Mutex<PendingSnapshotPackages>,
        snapshot_config: &SnapshotConfig,
    ) -> io::Result<()> {
        Self::purge_old_accounts_hashes(&accounts_package, snapshot_config);

        Self::submit_for_packaging(accounts_package, pending_snapshot_packages);

        Ok(())
    }

    fn purge_old_accounts_hashes(
        accounts_package: &AccountsPackage,
        snapshot_config: &SnapshotConfig,
    ) {
        let should_purge = match (
            snapshot_config.should_generate_snapshots(),
            accounts_package.package_kind,
        ) {
            (false, _) => {
                // If we are *not* generating snapshots, then it is safe to purge every time.
                true
            }
            (true, AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot)) => {
                // If we *are* generating snapshots, then only purge old accounts hashes after
                // handling full snapshot packages.  This is because handling incremental snapshot
                // packages requires the accounts hash from the latest full snapshot, and if we
                // purged after every package, we'd remove the accounts hash needed by the next
                // incremental snapshot.
                true
            }
            (true, _) => false,
        };

        if should_purge {
            accounts_package
                .accounts
                .accounts_db
                .purge_old_accounts_hashes(accounts_package.slot);
        }
    }

    fn submit_for_packaging(
        accounts_package: AccountsPackage,
        pending_snapshot_packages: &Mutex<PendingSnapshotPackages>,
    ) {
        if !matches!(
            accounts_package.package_kind,
            AccountsPackageKind::Snapshot(_)
        ) {
            return;
        }

        let snapshot_package = SnapshotPackage::new(accounts_package);
        pending_snapshot_packages
            .lock()
            .unwrap()
            .push(snapshot_package);
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_accounts_hash_verifier.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, rand::seq::SliceRandom, solana_clock::Slot,
        solana_runtime::snapshot_package::SnapshotKind,
    };

    fn new(package_kind: AccountsPackageKind, slot: Slot) -> AccountsPackage {
        AccountsPackage {
            package_kind,
            slot,
            block_height: slot,
            ..AccountsPackage::default_for_tests()
        }
    }
    fn new_fss(slot: Slot) -> AccountsPackage {
        new(
            AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot),
            slot,
        )
    }
    fn new_iss(slot: Slot, base: Slot) -> AccountsPackage {
        new(
            AccountsPackageKind::Snapshot(SnapshotKind::IncrementalSnapshot(base)),
            slot,
        )
    }

    /// Ensure that unhandled accounts packages are properly re-enqueued or dropped
    ///
    /// The accounts package handler should re-enqueue unhandled accounts packages, if those
    /// unhandled accounts packages are for slots GREATER-THAN the last handled accounts package.
    /// Otherwise, they should be dropped.
    #[test]
    fn test_get_next_accounts_package1() {
        let (accounts_package_sender, accounts_package_receiver) = crossbeam_channel::unbounded();

        // Populate the channel so that re-enqueueing and dropping will be tested
        let mut accounts_packages = [
            new_fss(100), // skipped, since there's another full snapshot with a higher slot
            new_iss(110, 100),
            new_iss(210, 100),
            new_fss(300), // skipped, since there's another full snapshot with a higher slot
            new_iss(310, 300),
            new_fss(400), // <-- handle 1st
            new_iss(410, 400),
            new_iss(420, 400), // <-- handle 2nd
        ];
        // Shuffle the accounts packages to simulate receiving new accounts packages from ABS
        // simultaneously as AHV is processing them.
        accounts_packages.shuffle(&mut rand::thread_rng());
        accounts_packages
            .into_iter()
            .for_each(|accounts_package| accounts_package_sender.send(accounts_package).unwrap());

        // The Full Snapshot from slot 400 is handled 1st
        // (the older full snapshots are skipped and dropped)
        let (
            account_package,
            _num_outstanding_accounts_packages,
            num_re_enqueued_accounts_packages,
        ) = AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver,
        )
        .unwrap();
        assert_eq!(
            account_package.package_kind,
            AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot)
        );
        assert_eq!(account_package.slot, 400);
        assert_eq!(num_re_enqueued_accounts_packages, 2);

        // The Incremental Snapshot from slot 420 is handled 3rd
        // (the older incremental snapshot from slot 410 is skipped and dropped)
        let (
            account_package,
            _num_outstanding_accounts_packages,
            num_re_enqueued_accounts_packages,
        ) = AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver,
        )
        .unwrap();
        assert_eq!(
            account_package.package_kind,
            AccountsPackageKind::Snapshot(SnapshotKind::IncrementalSnapshot(400))
        );
        assert_eq!(account_package.slot, 420);
        assert_eq!(num_re_enqueued_accounts_packages, 0);

        // And now the accounts package channel is empty!
        assert!(AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver
        )
        .is_none());
    }
}
