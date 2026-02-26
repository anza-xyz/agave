#![cfg(feature = "agave-unstable-api")]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

mod client_ids;
pub mod v1;
pub mod v2;
pub mod v3;
pub mod v4;

pub use {client_ids::*, v4::*};

pub(crate) fn compute_commit(sha1: Option<&'static str>) -> Option<u32> {
    u32::from_str_radix(sha1?.get(..8)?, /*radix:*/ 16).ok()
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
        &*format!("{:?}", $crate::Version::default())
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
}
