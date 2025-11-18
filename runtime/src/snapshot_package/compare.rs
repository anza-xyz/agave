use {
    super::{SnapshotArchiveKind, SnapshotKind, SnapshotPackage},
    std::cmp::Ordering::{self, Equal, Greater, Less},
};

/// Compare snapshot packages by priority; first by kind, then by slot
#[must_use]
pub fn cmp_snapshot_packages_by_priority(a: &SnapshotPackage, b: &SnapshotPackage) -> Ordering {
    cmp_snapshot_kinds_by_priority(&a.snapshot_kind, &b.snapshot_kind).then(a.slot.cmp(&b.slot))
}

/// Compare snapshot kinds by priority
#[must_use]
pub fn cmp_snapshot_kinds_by_priority(a: &SnapshotKind, b: &SnapshotKind) -> Ordering {
    use SnapshotKind as Kind;
    match (a, b) {
        (Kind::Archive(snapshot_archive_kind_a), Kind::Archive(snapshot_archive_kind_b)) => {
            cmp_snapshot_archive_kinds_by_priority(snapshot_archive_kind_a, snapshot_archive_kind_b)
        }
    }
}

/// Compare snapshot archive kinds by priority
///
/// Full snapshot archives are higher in priority than incremental snapshot archives.
/// If two `Incremental`s are compared, their base slots are the tiebreaker.
#[must_use]
pub fn cmp_snapshot_archive_kinds_by_priority(
    a: &SnapshotArchiveKind,
    b: &SnapshotArchiveKind,
) -> Ordering {
    use SnapshotArchiveKind as Kind;
    match (a, b) {
        (Kind::Full, Kind::Full) => Equal,
        (Kind::Full, Kind::Incremental(_)) => Greater,
        (Kind::Incremental(_), Kind::Full) => Less,
        (Kind::Incremental(base_slot_a), Kind::Incremental(base_slot_b)) => {
            base_slot_a.cmp(base_slot_b)
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_clock::Slot};

    #[test]
    fn test_cmp_snapshot_packages_by_priority() {
        fn new(snapshot_kind: SnapshotKind, slot: Slot) -> SnapshotPackage {
            SnapshotPackage {
                snapshot_kind,
                slot,
                block_height: slot,
                ..SnapshotPackage::default_for_tests()
            }
        }

        for (snapshot_package_a, snapshot_package_b, expected_result) in [
            (
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 11),
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                Less,
            ),
            (
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                Equal,
            ),
            (
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 33),
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                Greater,
            ),
            (
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(88)),
                    99,
                ),
                Greater,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(11)),
                    55,
                ),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    55,
                ),
                Less,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    55,
                ),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    55,
                ),
                Equal,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(33)),
                    55,
                ),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    55,
                ),
                Greater,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    44,
                ),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    55,
                ),
                Less,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    55,
                ),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    55,
                ),
                Equal,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    66,
                ),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(22)),
                    55,
                ),
                Greater,
            ),
        ] {
            let actual_result =
                cmp_snapshot_packages_by_priority(&snapshot_package_a, &snapshot_package_b);
            assert_eq!(expected_result, actual_result);
        }
    }

    #[test]
    fn test_cmp_snapshot_kinds_by_priority() {
        for (snapshot_kind_a, snapshot_kind_b, expected_result) in [
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                Equal,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                Greater,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                Less,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(6)),
                Less,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                Equal,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(4)),
                Greater,
            ),
        ] {
            let actual_result = cmp_snapshot_kinds_by_priority(&snapshot_kind_a, &snapshot_kind_b);
            assert_eq!(expected_result, actual_result);
        }
    }

    #[test]
    fn test_cmp_snapshot_archive_kinds_by_priority() {
        for (snapshot_archive_kind_a, snapshot_archive_kind_b, expected_result) in [
            (SnapshotArchiveKind::Full, SnapshotArchiveKind::Full, Equal),
            (
                SnapshotArchiveKind::Full,
                SnapshotArchiveKind::Incremental(5),
                Greater,
            ),
            (
                SnapshotArchiveKind::Incremental(5),
                SnapshotArchiveKind::Full,
                Less,
            ),
            (
                SnapshotArchiveKind::Incremental(5),
                SnapshotArchiveKind::Incremental(6),
                Less,
            ),
            (
                SnapshotArchiveKind::Incremental(5),
                SnapshotArchiveKind::Incremental(5),
                Equal,
            ),
            (
                SnapshotArchiveKind::Incremental(5),
                SnapshotArchiveKind::Incremental(4),
                Greater,
            ),
        ] {
            let actual_result = cmp_snapshot_archive_kinds_by_priority(
                &snapshot_archive_kind_a,
                &snapshot_archive_kind_b,
            );
            assert_eq!(expected_result, actual_result);
        }
    }
}
