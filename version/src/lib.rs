#![cfg(feature = "agave-unstable-api")]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

mod client_ids;
pub mod v3;
pub mod v4;

pub use {client_ids::*, v4::*};

pub(crate) fn compute_commit(sha1: Option<&'static str>) -> Option<u32> {
    u32::from_str_radix(sha1?.get(..8)?, /*radix:*/ 16).ok()
}

/// The build's Git commit (possibly abbreviated), or `None` if unavailable.
pub fn git_commit() -> Option<&'static str> {
    find_git_commit([
        option_env!("CI_COMMIT"),
        option_env!("AGAVE_GIT_COMMIT_HASH"),
    ])
}

fn find_git_commit(candidates: [Option<&str>; 2]) -> Option<&str> {
    candidates.into_iter().flatten().find(|sha| {
        (8..=64).contains(&sha.len()) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

#[macro_export]
macro_rules! semver {
    () => {
        &*format!("{}", $crate::Version::default())
    };
}

#[macro_export]
macro_rules! version {
    () => {
        &*format!("{}", $crate::Version::default().as_detailed_string())
    };
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_compute_commit() {
        assert_eq!(compute_commit(None), None);
        assert_eq!(compute_commit(Some("1234567890")), Some(0x1234_5678));
        assert_eq!(compute_commit(Some("HEAD")), None);
        assert_eq!(compute_commit(Some("garbagein")), None);
    }

    #[test]
    fn test_git_commit_metadata() {
        const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            find_git_commit([Some("abcdef01"), Some(HASH)]),
            Some("abcdef01")
        );
        assert_eq!(find_git_commit([None, Some(HASH)]), Some(HASH));
        assert_eq!(find_git_commit([None, None]), None);
        for invalid in [
            "",
            "HEAD",
            "garbagein",
            "1234567",
            "12345678garbage",
            &"a".repeat(65),
        ] {
            assert_eq!(find_git_commit([Some(invalid), Some(HASH)]), Some(HASH));
            assert_eq!(find_git_commit([None, Some(invalid)]), None);
        }
    }
}
