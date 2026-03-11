use {
    crate::compute_commit,
    serde::{Deserialize, Serialize},
    solana_sanitize::Sanitize,
    std::{convert::TryInto, fmt},
};

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    pub commit: Option<u32>, // first 4 bytes of the sha1 commit hash
    pub feature_set: u32,    // first 4 bytes of the FeatureSet identifier
}

impl Default for Version {
    fn default() -> Self {
        let feature_set =
            u32::from_le_bytes(agave_feature_set::ID.as_ref()[..4].try_into().unwrap());
        Self {
            major: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap(),
            minor: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap(),
            patch: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap(),
            commit: compute_commit(option_env!("CI_COMMIT")),
            feature_set,
        }
    }
}

impl fmt::Debug for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{} (src:{}; feat:{})",
            self.major,
            self.minor,
            self.patch,
            match self.commit {
                None => "devbuild".to_string(),
                Some(commit) => format!("{commit:08x}"),
            },
            self.feature_set,
        )
    }
}

impl Sanitize for Version {}
