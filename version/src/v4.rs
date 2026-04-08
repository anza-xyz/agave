use {
    crate::{client_ids::ClientId, compute_commit},
    rand::{Rng, rng},
    serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::Error as _},
    solana_sanitize::Sanitize,
    solana_serde_varint as serde_varint,
    std::{convert::TryInto, fmt, str::FromStr},
};

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prerelease {
    Stable,
    ReleaseCandidate(u16),
    Beta(u16),
    Alpha(u16),
}

impl Prerelease {
    const ENCODE_TAG_STABLE: u16 = 0;
    const ENCODE_TAG_RELEASE_CANDIDATE: u16 = 1;
    const ENCODE_TAG_BETA: u16 = 2;
    const ENCODE_TAG_ALPHA: u16 = 3;
    const IDENTIFIER_RELEASE_CANDIDATE: &str = "rc";
    const IDENTIFIER_BETA: &str = "beta";
    const IDENTIFIER_ALPHA: &str = "alpha";

    pub fn patch_is_valid(&self, patch: u16) -> bool {
        *self == Self::Stable || patch == 0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParsePrereleaseError {
    DotSeparatorMissing,
    NumericPartNotAU16,
    UnknownIdentifier,
}

impl fmt::Display for Prerelease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (identifier, number) = match &self {
            Self::Stable => return Ok(()),
            Self::ReleaseCandidate(rc) => (Self::IDENTIFIER_RELEASE_CANDIDATE, rc),
            Self::Beta(beta) => (Self::IDENTIFIER_BETA, beta),
            Self::Alpha(alpha) => (Self::IDENTIFIER_ALPHA, alpha),
        };
        write!(f, "{identifier}.{number}")
    }
}

impl FromStr for Prerelease {
    type Err = ParsePrereleaseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            Ok(Self::Stable)
        } else {
            let mut parts = s.rsplitn(2, '.');
            let num_part = parts
                .next()
                .expect("rsplitn returns at least one empty string");
            let identifier = parts
                .next()
                .ok_or(ParsePrereleaseError::DotSeparatorMissing)?;
            assert_eq!(
                parts.next(),
                None,
                "Safety: rsplitn(2, ...) produces no more than two parts"
            );
            let num =
                u16::from_str(num_part).map_err(|_| ParsePrereleaseError::NumericPartNotAU16)?;
            match identifier {
                Self::IDENTIFIER_RELEASE_CANDIDATE => Ok(Self::ReleaseCandidate(num)),
                Self::IDENTIFIER_BETA => Ok(Self::Beta(num)),
                Self::IDENTIFIER_ALPHA => Ok(Self::Alpha(num)),
                _ => Err(ParsePrereleaseError::UnknownIdentifier),
            }
        }
    }
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
struct PackedMinor(#[serde(with = "serde_varint")] u16);

#[derive(Clone, Debug, PartialEq)]
pub enum PackedMinorPackError {
    MinorTooLarge,
    InvalidPatchForPrerelease,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PackedMinorUnpackError {
    ReservedBitsSet,
}

impl PackedMinor {
    const PRERELEASE_BITS_OFFSET: u32 = 14;
    const PRERELEASE_MASK_BITS: u32 = 2;
    const PRERELEASE_FIRST_UNMASKED_BIT: u16 = 1 << Self::PRERELEASE_MASK_BITS;
    const PRERELEASE_MASK: u16 = Self::PRERELEASE_FIRST_UNMASKED_BIT - 1;
    const PRERELEASE_MINOR_MAX: u16 = (1 << Self::PRERELEASE_BITS_OFFSET) - 1;

    fn try_pack(
        minor: u16,
        patch: u16,
        prerelease: &Prerelease,
    ) -> Result<(Self, u16), PackedMinorPackError> {
        if minor > Self::PRERELEASE_MINOR_MAX {
            return Err(PackedMinorPackError::MinorTooLarge);
        }
        if !prerelease.patch_is_valid(patch) {
            return Err(PackedMinorPackError::InvalidPatchForPrerelease);
        }
        let (prerelease_encode_tag, patch) = match *prerelease {
            Prerelease::Stable => (Prerelease::ENCODE_TAG_STABLE, patch),
            Prerelease::ReleaseCandidate(rc) => (Prerelease::ENCODE_TAG_RELEASE_CANDIDATE, rc),
            Prerelease::Beta(beta) => (Prerelease::ENCODE_TAG_BETA, beta),
            Prerelease::Alpha(alpha) => (Prerelease::ENCODE_TAG_ALPHA, alpha),
        };
        let packed_minor = minor | prerelease_encode_tag << Self::PRERELEASE_BITS_OFFSET;
        Ok((Self(packed_minor), patch))
    }

    fn try_unpack(self, patch: u16) -> Result<(u16, u16, Prerelease), PackedMinorUnpackError> {
        let Self(packed_minor) = self;
        let shifted_prerelease_bits = packed_minor >> Self::PRERELEASE_BITS_OFFSET;

        let reserved_bits = shifted_prerelease_bits & !Self::PRERELEASE_MASK;
        if reserved_bits != 0 {
            return Err(PackedMinorUnpackError::ReservedBitsSet);
        }

        let prerelease_variant = shifted_prerelease_bits & Self::PRERELEASE_MASK;
        let minor = packed_minor & !(Self::PRERELEASE_MASK << Self::PRERELEASE_BITS_OFFSET);

        let (patch, prerelease) = match prerelease_variant {
            Prerelease::ENCODE_TAG_STABLE => (patch, Prerelease::Stable),
            Prerelease::ENCODE_TAG_RELEASE_CANDIDATE => (0, Prerelease::ReleaseCandidate(patch)),
            Prerelease::ENCODE_TAG_BETA => (0, Prerelease::Beta(patch)),
            Prerelease::ENCODE_TAG_ALPHA => (0, Prerelease::Alpha(patch)),
            Self::PRERELEASE_FIRST_UNMASKED_BIT..=u16::MAX => unreachable!(),
        };
        Ok((minor, patch, prerelease))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: u16,
    minor: u16,
    patch: u16,
    commit: u32,
    feature_set: u32,
    client: ClientId,
    prerelease: Prerelease,
}

impl Version {
    fn new_from_parts(
        major: u16,
        minor: u16,
        patch: u16,
        commit: u32,
        feature_set: u32,
        client: ClientId,
        prerelease: Prerelease,
    ) -> Self {
        assert!(prerelease.patch_is_valid(patch));
        Self {
            major,
            minor,
            patch,
            commit,
            feature_set,
            client,
            prerelease,
        }
    }

    pub fn this_build() -> Self {
        Self::new_from_parts(
            env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap(),
            env!("CARGO_PKG_VERSION_MINOR").parse().unwrap(),
            env!("CARGO_PKG_VERSION_PATCH").parse().unwrap(),
            compute_commit(option_env!("CI_COMMIT"))
                .or(compute_commit(option_env!("AGAVE_GIT_COMMIT_HASH")))
                .unwrap_or_else(|| rng().random::<u32>()),
            u32::from_le_bytes(agave_feature_set::ID.as_ref()[..4].try_into().unwrap()),
            ClientId::this_client(),
            Prerelease::from_str(env!("CARGO_PKG_VERSION_PRE")).unwrap(),
        )
    }

    pub fn major(&self) -> u16 {
        self.major
    }

    pub fn minor(&self) -> u16 {
        self.minor
    }

    pub fn patch(&self) -> u16 {
        self.patch
    }

    pub fn commit(&self) -> u32 {
        self.commit
    }

    pub fn feature_set(&self) -> u32 {
        self.feature_set
    }

    pub fn client(&self) -> &ClientId {
        &self.client
    }

    pub fn prerelease(&self) -> &Prerelease {
        &self.prerelease
    }

    pub fn as_semver_string(&self) -> String {
        format!("{self}")
    }

    pub fn as_detailed_string(&self) -> String {
        format!(
            "{} (src:{:08x}; feat:{:08x}, client:{:?})",
            self, self.commit, self.feature_set, self.client,
        )
    }

    pub fn as_semver_version(&self) -> semver::Version {
        let major = u64::from(self.major);
        let minor = u64::from(self.minor);
        let patch = u64::from(self.patch);
        let pre = semver::Prerelease::new(&self.prerelease.to_string())
            .expect("solana_version::Prerelease is semver::Prerelease-compatible");
        let build = semver::BuildMetadata::EMPTY;
        semver::Version {
            major,
            minor,
            patch,
            pre,
            build,
        }
    }
}

impl Default for Version {
    fn default() -> Self {
        Self::this_build()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sep = if self.prerelease == Prerelease::Stable {
            ""
        } else {
            "-"
        };
        write!(
            f,
            "{}.{}.{}{}{}",
            self.major, self.minor, self.patch, sep, self.prerelease
        )
    }
}

impl Sanitize for Version {}
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Deserialize, Serialize)]
struct SerializedVersion {
    #[serde(with = "serde_varint")]
    major: u16,
    #[serde(rename = "minor")]
    packed_minor: PackedMinor,
    #[serde(with = "serde_varint")]
    patch: u16,
    commit: u32,
    feature_set: u32,
    #[serde(with = "serde_varint")]
    client: u16,
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let &Version {
            major,
            minor,
            patch,
            commit,
            feature_set,
            ref client,
            ref prerelease,
        } = self;

        let (packed_minor, patch) = PackedMinor::try_pack(minor, patch, prerelease)
            .map_err(|err| S::Error::custom(format!("{err:?}")))?;
        let client = u16::try_from(client.clone()).map_err(S::Error::custom)?;

        let serialized_version = SerializedVersion {
            major,
            packed_minor,
            patch,
            commit,
            feature_set,
            client,
        };

        SerializedVersion::serialize(&serialized_version, serializer)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Version, D::Error>
    where
        D: Deserializer<'de>,
    {
        let SerializedVersion {
            major,
            packed_minor,
            patch,
            commit,
            feature_set,
            client,
        } = SerializedVersion::deserialize(deserializer)?;

        let (minor, patch, prerelease) = packed_minor
            .try_unpack(patch)
            .map_err(|err| D::Error::custom(format!("{err:?}")))?;
        let client = ClientId::from(client);

        Ok(Version {
            major,
            minor,
            patch,
            commit,
            feature_set,
            client,
            prerelease,
        })
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::v3};

    #[test]
    fn test_prerelease_patch_is_valid() {
        assert!(Prerelease::Alpha(0).patch_is_valid(0));
        assert!(Prerelease::Beta(0).patch_is_valid(0));
        assert!(Prerelease::ReleaseCandidate(0).patch_is_valid(0));
        assert!(Prerelease::Stable.patch_is_valid(0));
        assert!(Prerelease::Stable.patch_is_valid(1));
        assert!(Prerelease::Stable.patch_is_valid(u16::MAX));

        assert!(!Prerelease::ReleaseCandidate(0).patch_is_valid(1));
        assert!(!Prerelease::Beta(0).patch_is_valid(1));
        assert!(!Prerelease::Alpha(0).patch_is_valid(1));
    }

    #[test]
    fn test_prerelease_display() {
        assert!(format!("{}", Prerelease::Stable).is_empty());
        assert_eq!(format!("{}", Prerelease::ReleaseCandidate(0)), "rc.0",);
        assert_eq!(format!("{}", Prerelease::Beta(1)), "beta.1",);
        assert_eq!(format!("{}", Prerelease::Alpha(2)), "alpha.2",);
        assert_eq!(format!("{}", Prerelease::Alpha(u16::MAX)), "alpha.65535",);
    }

    #[test]
    fn test_prerelease_from_str() {
        assert_eq!(Prerelease::from_str(""), Ok(Prerelease::Stable));
        assert_eq!(
            Prerelease::from_str("rc.0"),
            Ok(Prerelease::ReleaseCandidate(0))
        );
        assert_eq!(Prerelease::from_str("beta.1"), Ok(Prerelease::Beta(1)));
        assert_eq!(Prerelease::from_str("alpha.2"), Ok(Prerelease::Alpha(2)));
        assert_eq!(
            Prerelease::from_str("alpha.65535"),
            Ok(Prerelease::Alpha(u16::MAX))
        );
        assert_eq!(
            Prerelease::from_str("rc0"),
            Err(ParsePrereleaseError::DotSeparatorMissing),
        );
        assert_eq!(
            Prerelease::from_str("rc.a"),
            Err(ParsePrereleaseError::NumericPartNotAU16),
        );
        assert_eq!(
            Prerelease::from_str("rc.-1"),
            Err(ParsePrereleaseError::NumericPartNotAU16),
        );
        assert_eq!(
            Prerelease::from_str("rc.65536"),
            Err(ParsePrereleaseError::NumericPartNotAU16),
        );
        assert_eq!(
            Prerelease::from_str("unknown.0"),
            Err(ParsePrereleaseError::UnknownIdentifier),
        );
    }

    #[test]
    fn test_prerelease_roundtrips() {
        let test_cases = [
            ("", Prerelease::Stable),
            ("rc.0", Prerelease::ReleaseCandidate(0)),
            ("beta.1", Prerelease::Beta(1)),
            ("alpha.2", Prerelease::Alpha(2)),
        ];
        for (test_str, test_pr) in test_cases.into_iter() {
            let gen_pr = Prerelease::from_str(test_str).unwrap();
            let gen_str = test_pr.to_string();
            let regen_pr = Prerelease::from_str(&gen_str).unwrap();
            let regen_str = gen_pr.to_string();

            assert_eq!(test_str, gen_str);
            assert_eq!(test_str, regen_str);
            assert_eq!(test_pr, gen_pr);
            assert_eq!(test_pr, regen_pr);
        }
    }

    #[test]
    fn test_prerelease_compatible_with_semver_prerelease() {
        let test_cases = [
            ("", Prerelease::Stable),
            ("rc.0", Prerelease::ReleaseCandidate(0)),
            ("beta.1", Prerelease::Beta(1)),
            ("alpha.2", Prerelease::Alpha(2)),
        ];
        for (test_str, test_pr) in test_cases.into_iter() {
            let svpr = semver::Prerelease::new(test_pr.to_string().as_str()).unwrap();
            assert_eq!(svpr.as_str(), test_str);
        }
    }

    #[test]
    fn test_packed_minor_try_pack() {
        assert_eq!(
            PackedMinor::try_pack(0, 0, &Prerelease::Stable),
            Ok((PackedMinor(0), 0)),
        );
        assert_eq!(
            PackedMinor::try_pack(
                PackedMinor::PRERELEASE_MINOR_MAX,
                u16::MAX,
                &Prerelease::Stable,
            ),
            Ok((PackedMinor(0x3FFF), u16::MAX)),
        );
        assert_eq!(
            PackedMinor::try_pack(0, 0, &Prerelease::ReleaseCandidate(0)),
            Ok((PackedMinor(0x4000), 0)),
        );
        assert_eq!(
            PackedMinor::try_pack(
                PackedMinor::PRERELEASE_MINOR_MAX,
                0,
                &Prerelease::ReleaseCandidate(u16::MAX),
            ),
            Ok((PackedMinor(0x7FFF), u16::MAX)),
        );
        assert_eq!(
            PackedMinor::try_pack(0, 0, &Prerelease::Beta(0)),
            Ok((PackedMinor(0x8000), 0)),
        );
        assert_eq!(
            PackedMinor::try_pack(
                PackedMinor::PRERELEASE_MINOR_MAX,
                0,
                &Prerelease::Beta(u16::MAX),
            ),
            Ok((PackedMinor(0xBFFF), u16::MAX)),
        );
        assert_eq!(
            PackedMinor::try_pack(0, 0, &Prerelease::Alpha(0)),
            Ok((PackedMinor(0xC000), 0)),
        );
        assert_eq!(
            PackedMinor::try_pack(
                PackedMinor::PRERELEASE_MINOR_MAX,
                0,
                &Prerelease::Alpha(u16::MAX),
            ),
            Ok((PackedMinor(0xFFFF), u16::MAX)),
        );

        assert_eq!(
            PackedMinor::try_pack(
                PackedMinor::PRERELEASE_MINOR_MAX + 1,
                0,
                &Prerelease::Stable
            ),
            Err(PackedMinorPackError::MinorTooLarge),
        );
        assert_eq!(
            PackedMinor::try_pack(0, 1, &Prerelease::Beta(0),),
            Err(PackedMinorPackError::InvalidPatchForPrerelease),
        );
    }

    #[test]
    fn test_packed_minor_try_unpack() {
        assert_eq!(
            PackedMinor(0x0000).try_unpack(0),
            Ok((0, 0, Prerelease::Stable))
        );
        assert_eq!(
            PackedMinor(0x3FFF).try_unpack(u16::MAX),
            Ok((
                PackedMinor::PRERELEASE_MINOR_MAX,
                u16::MAX,
                Prerelease::Stable
            )),
        );
        assert_eq!(
            PackedMinor(0x4000).try_unpack(0),
            Ok((0, 0, Prerelease::ReleaseCandidate(0)))
        );
        assert_eq!(
            PackedMinor(0x7FFF).try_unpack(u16::MAX),
            Ok((
                PackedMinor::PRERELEASE_MINOR_MAX,
                0,
                Prerelease::ReleaseCandidate(u16::MAX)
            )),
        );
        assert_eq!(
            PackedMinor(0x8000).try_unpack(0),
            Ok((0, 0, Prerelease::Beta(0)))
        );
        assert_eq!(
            PackedMinor(0xBFFF).try_unpack(u16::MAX),
            Ok((
                PackedMinor::PRERELEASE_MINOR_MAX,
                0,
                Prerelease::Beta(u16::MAX)
            )),
        );
        assert_eq!(
            PackedMinor(0xC000).try_unpack(0),
            Ok((0, 0, Prerelease::Alpha(0)))
        );
        assert_eq!(
            PackedMinor(0xFFFF).try_unpack(u16::MAX),
            Ok((
                PackedMinor::PRERELEASE_MINOR_MAX,
                0,
                Prerelease::Alpha(u16::MAX)
            )),
        );

        // minor bits above the the prerelease mask are reserved. test that
        // the aren't set if they exist
        if PackedMinor::PRERELEASE_BITS_OFFSET + PackedMinor::PRERELEASE_MASK_BITS < u16::BITS {
            unimplemented!("current configuration leaves no bits reserved");
        }
    }

    fn prerelease_stable(_v: u16, patch: u16) -> (Prerelease, u16) {
        (Prerelease::Stable, patch)
    }

    fn prerelease_release_candidate(v: u16, _patch: u16) -> (Prerelease, u16) {
        (Prerelease::ReleaseCandidate(v), 0)
    }

    fn prerelease_beta(v: u16, _patch: u16) -> (Prerelease, u16) {
        (Prerelease::Beta(v), 0)
    }

    fn prerelease_alpha(v: u16, _patch: u16) -> (Prerelease, u16) {
        (Prerelease::Alpha(v), 0)
    }

    #[test]
    fn test_packed_minor_roundtrips() {
        let test_cases = [
            prerelease_stable,
            prerelease_release_candidate,
            prerelease_beta,
            prerelease_alpha,
        ];
        for prerelease_ctor in &test_cases {
            for minor in [0, 1, PackedMinor::PRERELEASE_MINOR_MAX] {
                for patch in [0, 1, u16::MAX] {
                    for prerelease in [0, 1, u16::MAX] {
                        let (prerelease, patch) = prerelease_ctor(prerelease, patch);
                        let (packed_minor, packed_patch) =
                            PackedMinor::try_pack(minor, patch, &prerelease).unwrap();
                        let unpacked = packed_minor.try_unpack(packed_patch).unwrap();
                        assert_eq!((minor, patch, prerelease), unpacked);
                    }
                }
            }
        }
    }

    #[test]
    fn test_v3_and_v4_same_size() {
        // smallest
        let v3_version = v3::Version {
            major: 0,
            minor: 0,
            patch: 0,
            commit: 0,
            feature_set: 0,
            client: 0,
        };
        let v4_version =
            Version::new_from_parts(0, 0, 0, 0, 0, ClientId::Agave, Prerelease::Stable);
        assert_eq!(
            bincode::serialized_size(&v3_version).unwrap(),
            bincode::serialized_size(&v4_version).unwrap(),
        );

        // largest
        let v3_version = v3::Version {
            major: u16::MAX,
            minor: u16::MAX,
            patch: u16::MAX,
            commit: u32::MAX,
            feature_set: u32::MAX,
            client: u16::MAX,
        };
        let v4_version = Version::new_from_parts(
            u16::MAX,
            PackedMinor::PRERELEASE_MINOR_MAX,
            0,
            u32::MAX,
            u32::MAX,
            ClientId::Unknown(u16::MAX),
            Prerelease::Alpha(u16::MAX),
        );
        assert_eq!(
            bincode::serialized_size(&v3_version).unwrap(),
            bincode::serialized_size(&v4_version).unwrap(),
        );
    }

    #[test]
    fn test_serde() {
        let version =
            Version::new_from_parts(0, 0, 0, 0, 0, ClientId::SolanaLabs, Prerelease::Stable);

        let bytes = bincode::serialize(&version).unwrap();
        assert_eq!(bytes, [0u8; 12]);

        let de_version: Version = bincode::deserialize(&bytes).unwrap();
        assert_eq!(version, de_version);

        let version = Version::new_from_parts(
            u16::MAX,
            PackedMinor::PRERELEASE_MINOR_MAX,
            0,
            u32::MAX,
            u32::MAX,
            ClientId::Unknown(u16::MAX),
            Prerelease::Alpha(u16::MAX),
        );

        let bytes = bincode::serialize(&version).unwrap();
        assert_eq!(
            bytes,
            [
                0xff, 0xff, 0x03, 0xff, 0xff, 0x03, 0xff, 0xff, 0x03, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0x03,
            ]
        );

        let de_version: Version = bincode::deserialize(&bytes).unwrap();
        assert_eq!(version, de_version);
    }

    #[test]
    fn test_version_as_detailed_string() {
        let version =
            Version::new_from_parts(0, 0, 0, 0, 0, ClientId::this_client(), Prerelease::Stable);
        assert_eq!(
            version.as_detailed_string(),
            "0.0.0 (src:00000000; feat:00000000, client:Agave)",
        );

        let version = Version::new_from_parts(
            0,
            0,
            0,
            0,
            0,
            ClientId::this_client(),
            Prerelease::ReleaseCandidate(0),
        );
        assert_eq!(
            version.as_detailed_string(),
            "0.0.0-rc.0 (src:00000000; feat:00000000, client:Agave)",
        );
    }
}
