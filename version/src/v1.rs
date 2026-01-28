use {
    serde::{Deserialize, Serialize},
    solana_sanitize::Sanitize,
};

// Older version structure used earlier 1.3.x releases
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Version {
    major: u16,
    minor: u16,
    patch: u16,
    commit: Option<u32>, // first 4 bytes of the sha1 commit hash
}

impl Sanitize for Version {}
