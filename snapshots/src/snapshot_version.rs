use std::{fmt, str::FromStr};

const VERSION_STRING_V1_2_0: &str = "1.2.0";

#[derive(Copy, Clone, Default, Eq, PartialEq, Debug)]
pub enum SnapshotVersion {
    #[default]
    V1_2_0,
}

impl fmt::Display for SnapshotVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(From::from(*self))
    }
}

impl From<SnapshotVersion> for &'static str {
    fn from(snapshot_version: SnapshotVersion) -> &'static str {
        match snapshot_version {
            SnapshotVersion::V1_2_0 => VERSION_STRING_V1_2_0,
        }
    }
}

impl FromStr for SnapshotVersion {
    type Err = &'static str;

    fn from_str(version_string: &str) -> Result<Self, Self::Err> {
        // Remove leading 'v' or 'V' from slice
        let version_string = if version_string
            .get(..1)
            .is_some_and(|s| s.eq_ignore_ascii_case("v"))
        {
            &version_string[1..]
        } else {
            version_string
        };
        match version_string {
            VERSION_STRING_V1_2_0 => Ok(SnapshotVersion::V1_2_0),
            _ => Err("unsupported snapshot version"),
        }
    }
}

impl SnapshotVersion {
    pub fn as_str(self) -> &'static str {
        <&str as From<Self>>::from(self)
    }
}
