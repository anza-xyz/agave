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
        (Kind::Archive(_), Kind::Fastboot) => Greater,
        (Kind::Fastboot, Kind::Archive(_)) => Less,
        (Kind::Fastboot, Kind::Fastboot) => Equal,
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

/// Check if two snapshots packages are the same kind
#[must_use]
pub fn are_snapshot_packages_the_same_kind(a: &SnapshotPackage, b: &SnapshotPackage) -> bool {
    are_snapshot_kinds_the_same_kind(&a.snapshot_kind, &b.snapshot_kind)
}

/// Check if two snapshot kinds are the same kind
#[must_use]
pub fn are_snapshot_kinds_the_same_kind(a: &SnapshotKind, b: &SnapshotKind) -> bool {
    use SnapshotKind as Kind;
    match (a, b) {
        (Kind::Archive(archive_a), Kind::Archive(archive_b)) => {
            are_snapshot_archive_kinds_the_same_kind(archive_a, archive_b)
        }
        (Kind::Archive(_), Kind::Fastboot) => false,
        (Kind::Fastboot, Kind::Archive(_)) => false,
        (Kind::Fastboot, Kind::Fastboot) => true,
    }
}

/// Check if two snapshot archives are the same kind
///
/// Incremental snapshot archives with different base slots are considered the same kind
#[must_use]
pub fn are_snapshot_archive_kinds_the_same_kind(
    a: &SnapshotArchiveKind,
    b: &SnapshotArchiveKind,
) -> bool {
    use SnapshotArchiveKind as Kind;
    match (a, b) {
        (Kind::Full, Kind::Full) => true,
        (Kind::Full, Kind::Incremental(_)) => false,
        (Kind::Incremental(_), Kind::Full) => false,
        (Kind::Incremental(_), Kind::Incremental(_)) => true,
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_clock::Slot};

    fn new(snapshot_kind: SnapshotKind, slot: Slot) -> SnapshotPackage {
        SnapshotPackage {
            snapshot_kind,
            slot,
            ..SnapshotPackage::default_for_tests()
        }
    }

    #[test]
    fn test_cmp_snapshot_packages_by_priority() {
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
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                new(SnapshotKind::Fastboot, 33),
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
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(11)),
                    22,
                ),
                new(SnapshotKind::Fastboot, 33),
                Greater,
            ),
            (
                new(SnapshotKind::Fastboot, 22),
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 11),
                Less,
            ),
            (
                new(SnapshotKind::Fastboot, 33),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(11)),
                    22,
                ),
                Less,
            ),
            (
                new(SnapshotKind::Fastboot, 11),
                new(SnapshotKind::Fastboot, 22),
                Less,
            ),
            (
                new(SnapshotKind::Fastboot, 22),
                new(SnapshotKind::Fastboot, 22),
                Equal,
            ),
            (
                new(SnapshotKind::Fastboot, 33),
                new(SnapshotKind::Fastboot, 22),
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
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                SnapshotKind::Fastboot,
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
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                SnapshotKind::Fastboot,
                Greater,
            ),
            (
                SnapshotKind::Fastboot,
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                Less,
            ),
            (
                SnapshotKind::Fastboot,
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                Less,
            ),
            (SnapshotKind::Fastboot, SnapshotKind::Fastboot, Equal),
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

    #[test]
    fn test_are_snapshot_packages_the_same_kind() {
        for (snapshot_package_a, snapshot_package_b, expected_result) in [
            (
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 11),
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                true,
            ),
            (
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 11),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                    22,
                ),
                false,
            ),
            (
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                new(SnapshotKind::Fastboot, 11),
                false,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                    11,
                ),
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                false,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                    11,
                ),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(6)),
                    22,
                ),
                true,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                    11,
                ),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                    22,
                ),
                true,
            ),
            (
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                    22,
                ),
                new(SnapshotKind::Fastboot, 11),
                false,
            ),
            (
                new(SnapshotKind::Fastboot, 11),
                new(SnapshotKind::Archive(SnapshotArchiveKind::Full), 22),
                false,
            ),
            (
                new(SnapshotKind::Fastboot, 11),
                new(
                    SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                    22,
                ),
                false,
            ),
            (
                new(SnapshotKind::Fastboot, 11),
                new(SnapshotKind::Fastboot, 22),
                true,
            ),
        ] {
            let actual_result =
                are_snapshot_packages_the_same_kind(&snapshot_package_a, &snapshot_package_b);
            assert_eq!(expected_result, actual_result);
        }
    }
    #[test]
    fn test_are_snapshot_kinds_the_same_kind() {
        for (snapshot_kind_a, snapshot_kind_b, expected_result) in [
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                true,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                false,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                SnapshotKind::Fastboot,
                false,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                false,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(6)),
                true,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                true,
            ),
            (
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                SnapshotKind::Fastboot,
                false,
            ),
            (
                SnapshotKind::Fastboot,
                SnapshotKind::Archive(SnapshotArchiveKind::Full),
                false,
            ),
            (
                SnapshotKind::Fastboot,
                SnapshotKind::Archive(SnapshotArchiveKind::Incremental(5)),
                false,
            ),
            (SnapshotKind::Fastboot, SnapshotKind::Fastboot, true),
        ] {
            let actual_result =
                are_snapshot_kinds_the_same_kind(&snapshot_kind_a, &snapshot_kind_b);
            assert_eq!(expected_result, actual_result);
        }
    }
}
