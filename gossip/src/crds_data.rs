#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample, frozen_abi};
use {
    crate::contact_info::ContactInfo,
    rand::Rng,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    solana_time_utils::timestamp,
    wincode::{SchemaRead, SchemaWrite},
};

pub(crate) const MAX_WALLCLOCK: u64 = 1_000_000_000_000_000;
pub(crate) const MAX_SLOT: u64 = 1_000_000_000_000_000;

#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite)]
pub(crate) struct Deprecated {}

unsafe impl<'de, C: wincode::config::Config> SchemaRead<'de, C> for Deprecated {
    type Dst = Self;
    fn read(
        _reader: impl wincode::io::Reader<'de>,
        _dst: &mut std::mem::MaybeUninit<Self>,
    ) -> wincode::ReadResult<()> {
        Err(wincode::ReadError::Custom(
            "Trying to deserialize deprecated type",
        ))
    }
}

/// Values replicated through gossip. Deprecated variants reserve their original
/// wire discriminants and reject deserialization before reading their payloads.
#[allow(clippy::large_enum_variant, private_interfaces)]
#[cfg_attr(
    feature = "stable-abi",
    derive(StableAbi),
    frozen_abi(
        abi_digest = "FFWhSyi3qS613LibmpWeybDnyxxxuZ6qFwicgPzNR7wv",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[cfg_attr(test, derive(strum_macros::EnumCount))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
pub enum CrdsData {
    LegacyContactInfo(Deprecated),    // Deprecated
    Vote(Deprecated),                 // Deprecated
    LowestSlot(Deprecated),           // Deprecated
    LegacySnapshotHashes(Deprecated), // Deprecated
    AccountsHashes(Deprecated),       // Deprecated
    EpochSlots(Deprecated),           // Deprecated
    LegacyVersion(Deprecated),        // Deprecated
    Version(Deprecated),              // Deprecated
    NodeInstance(Deprecated),         // Deprecated
    DuplicateShred(Deprecated),       // Deprecated
    SnapshotHashes(SnapshotHashes),
    ContactInfo(ContactInfo),
    RestartLastVotedForkSlots(Deprecated), // Deprecated
    RestartHeaviestFork(Deprecated),       // Deprecated
}

impl Sanitize for CrdsData {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        match self {
            Self::SnapshotHashes(value) => value.sanitize(),
            Self::ContactInfo(value) => value.sanitize(),
            Self::LegacyContactInfo(_)
            | Self::Vote(_)
            | Self::LowestSlot(_)
            | Self::LegacySnapshotHashes(_)
            | Self::AccountsHashes(_)
            | Self::EpochSlots(_)
            | Self::LegacyVersion(_)
            | Self::Version(_)
            | Self::NodeInstance(_)
            | Self::DuplicateShred(_)
            | Self::RestartLastVotedForkSlots(_)
            | Self::RestartHeaviestFork(_) => Err(SanitizeError::InvalidValue),
        }
    }
}

/// Random timestamp for tests and benchmarks.
pub(crate) fn new_rand_timestamp<R: Rng>(rng: &mut R) -> u64 {
    const DELAY: u64 = 10 * 60 * 1000;
    timestamp() - DELAY + rng.random_range(0..2 * DELAY)
}

impl CrdsData {
    /// New random CrdsData for tests and benchmarks.
    pub(crate) fn new_rand<R: Rng>(rng: &mut R, pubkey: Option<Pubkey>) -> Self {
        if rng.random() {
            Self::from(ContactInfo::new_rand(rng, pubkey))
        } else {
            Self::SnapshotHashes(SnapshotHashes {
                from: pubkey.unwrap_or_else(solana_pubkey::new_rand),
                full: (
                    rng.random_range(0..MAX_SLOT),
                    Hash::new_from_array(rng.random()),
                ),
                incremental: vec![],
                wallclock: new_rand_timestamp(rng),
            })
        }
    }

    pub(crate) fn wallclock(&self) -> u64 {
        match self {
            Self::SnapshotHashes(value) => value.wallclock,
            Self::ContactInfo(value) => value.wallclock(),
            Self::LegacyContactInfo(_)
            | Self::Vote(_)
            | Self::LowestSlot(_)
            | Self::LegacySnapshotHashes(_)
            | Self::AccountsHashes(_)
            | Self::EpochSlots(_)
            | Self::LegacyVersion(_)
            | Self::Version(_)
            | Self::NodeInstance(_)
            | Self::DuplicateShred(_)
            | Self::RestartLastVotedForkSlots(_)
            | Self::RestartHeaviestFork(_) => unreachable!("deprecated CrdsData variant"),
        }
    }

    pub(crate) fn pubkey(&self) -> Pubkey {
        match self {
            Self::SnapshotHashes(value) => value.from,
            Self::ContactInfo(value) => *value.pubkey(),
            Self::LegacyContactInfo(_)
            | Self::Vote(_)
            | Self::LowestSlot(_)
            | Self::LegacySnapshotHashes(_)
            | Self::AccountsHashes(_)
            | Self::EpochSlots(_)
            | Self::LegacyVersion(_)
            | Self::Version(_)
            | Self::NodeInstance(_)
            | Self::DuplicateShred(_)
            | Self::RestartLastVotedForkSlots(_)
            | Self::RestartHeaviestFork(_) => unreachable!("deprecated CrdsData variant"),
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn is_deprecated(&self) -> bool {
        match self {
            Self::SnapshotHashes(_) | Self::ContactInfo(_) => false,
            Self::LegacyContactInfo(_)
            | Self::Vote(_)
            | Self::LowestSlot(_)
            | Self::LegacySnapshotHashes(_)
            | Self::AccountsHashes(_)
            | Self::EpochSlots(_)
            | Self::LegacyVersion(_)
            | Self::Version(_)
            | Self::NodeInstance(_)
            | Self::DuplicateShred(_)
            | Self::RestartLastVotedForkSlots(_)
            | Self::RestartHeaviestFork(_) => true,
        }
    }
}

impl From<ContactInfo> for CrdsData {
    fn from(node: ContactInfo) -> Self {
        Self::ContactInfo(node)
    }
}
impl From<&ContactInfo> for CrdsData {
    fn from(node: &ContactInfo) -> Self {
        Self::ContactInfo(node.clone())
    }
}

#[cfg_attr(
    feature = "stable-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "EMCUxeoxyA9ATkmZpACEx6a6mPd5nV16g9vpeuFzpyG5",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct SnapshotHashes {
    pub from: Pubkey,
    pub full: (Slot, Hash),
    pub incremental: Vec<(Slot, Hash)>,
    pub wallclock: u64,
}

impl Sanitize for SnapshotHashes {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        sanitize_wallclock(self.wallclock)?;
        if self.full.0 >= MAX_SLOT {
            return Err(SanitizeError::ValueOutOfBounds);
        }
        for (slot, _) in &self.incremental {
            if *slot >= MAX_SLOT {
                return Err(SanitizeError::ValueOutOfBounds);
            }
            if self.full.0 >= *slot {
                return Err(SanitizeError::InvalidValue);
            }
        }
        self.from.sanitize()
    }
}

pub(crate) fn sanitize_wallclock(wallclock: u64) -> Result<(), SanitizeError> {
    if wallclock >= MAX_WALLCLOCK {
        Err(SanitizeError::ValueOutOfBounds)
    } else {
        Ok(())
    }
}

// Sample only the variants that can be decoded for ABI round-trip tests.
#[cfg(feature = "stable-abi")]
impl solana_frozen_abi::rand::distr::Distribution<CrdsData>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> CrdsData {
        use solana_frozen_abi::stable_abi::StableAbi;
        if rng.random() {
            CrdsData::SnapshotHashes(<SnapshotHashes as StableAbi>::random(rng))
        } else {
            CrdsData::ContactInfo(<ContactInfo as StableAbi>::random(rng))
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, strum::EnumCount};

    #[test]
    fn test_deprecated_values_reject_deserialization() {
        let values = [
            CrdsData::LegacyContactInfo(Deprecated {}),
            CrdsData::Vote(Deprecated {}),
            CrdsData::LowestSlot(Deprecated {}),
            CrdsData::LegacySnapshotHashes(Deprecated {}),
            CrdsData::AccountsHashes(Deprecated {}),
            CrdsData::EpochSlots(Deprecated {}),
            CrdsData::LegacyVersion(Deprecated {}),
            CrdsData::Version(Deprecated {}),
            CrdsData::NodeInstance(Deprecated {}),
            CrdsData::DuplicateShred(Deprecated {}),
            CrdsData::RestartLastVotedForkSlots(Deprecated {}),
            CrdsData::RestartHeaviestFork(Deprecated {}),
        ];
        assert_eq!(values.len() + 2, CrdsData::COUNT);
        for value in values {
            assert!(value.is_deprecated());
            assert_eq!(value.sanitize(), Err(SanitizeError::InvalidValue));
            let bytes = wincode::serialize(&value).unwrap();
            assert!(wincode::deserialize::<CrdsData>(&bytes).is_err());
            // Historic payloads are also rejected without parsing them.
            let mut bytes = bytes;
            bytes.extend_from_slice(&[0; 256]);
            assert!(wincode::deserialize::<CrdsData>(&bytes).is_err());
        }
    }

    #[test]
    fn test_retained_wire_discriminants() {
        let values = [
            (11u32, CrdsData::from(ContactInfo::default())),
            (
                10,
                CrdsData::SnapshotHashes(SnapshotHashes {
                    from: Pubkey::new_unique(),
                    full: (1, Hash::new_unique()),
                    incremental: vec![],
                    wallclock: timestamp(),
                }),
            ),
        ];
        for (discriminant, value) in values {
            assert!(!value.is_deprecated());
            value.sanitize().unwrap();
            let bytes = wincode::serialize(&value).unwrap();
            assert_eq!(&bytes[..4], &discriminant.to_le_bytes());
            assert_eq!(wincode::deserialize::<CrdsData>(&bytes).unwrap(), value);
        }
    }
}
