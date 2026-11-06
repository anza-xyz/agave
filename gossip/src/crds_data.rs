#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample, frozen_abi};
use {
    crate::{
        contact_info::ContactInfo,
        duplicate_shred::{DuplicateShred, DuplicateShredIndex, MAX_DUPLICATE_SHREDS},
        restart_crds_values::{RestartHeaviestFork, RestartLastVotedForkSlots},
    },
    rand::Rng,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::{self, Pubkey},
    solana_sanitize::{Sanitize, SanitizeError},
    solana_time_utils::timestamp,
    wincode::{SchemaRead, SchemaWrite},
};

pub(crate) const MAX_WALLCLOCK: u64 = 1_000_000_000_000_000;
pub(crate) const MAX_SLOT: u64 = 1_000_000_000_000_000;

// Helper for deprecated types
#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite)]
pub(crate) struct Deprecated {}
reject_deserialize!(Deprecated, "Trying to deserialize deprecated type");

/// CrdsData that defines the different types of items CrdsValues can hold
/// * Merge Strategy - Latest wallclock is picked
#[allow(clippy::large_enum_variant)]
#[cfg_attr(
    feature = "stable-abi",
    derive(StableAbi),
    frozen_abi(
        abi_digest = "7ER4fAKxdgFXbTaio7KwBwhpE4WN2SBtVt51g4NWgom2",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[cfg_attr(test, derive(strum_macros::EnumCount))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
pub enum CrdsData {
    #[allow(private_interfaces)]
    LegacyContactInfo(Deprecated), // Deprecated
    #[allow(private_interfaces)]
    Vote(Deprecated), // Deprecated
    #[allow(private_interfaces)]
    LowestSlot(Deprecated), // Deprecated
    #[allow(private_interfaces)]
    LegacySnapshotHashes(Deprecated), // Deprecated
    #[allow(private_interfaces)]
    AccountsHashes(Deprecated), // Deprecated
    #[allow(private_interfaces)]
    EpochSlots(Deprecated), // Deprecated
    #[allow(private_interfaces)]
    LegacyVersion(Deprecated), // Deprecated
    #[allow(private_interfaces)]
    Version(Deprecated), // Deprecated
    #[allow(private_interfaces)]
    NodeInstance(Deprecated), // Deprecated
    DuplicateShred(DuplicateShredIndex, DuplicateShred),
    SnapshotHashes(SnapshotHashes),
    ContactInfo(ContactInfo),
    RestartLastVotedForkSlots(RestartLastVotedForkSlots), // Deprecated
    RestartHeaviestFork(RestartHeaviestFork),             // Deprecated
}

impl Sanitize for CrdsData {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        match self {
            CrdsData::DuplicateShred(ix, shred) => {
                if *ix >= MAX_DUPLICATE_SHREDS {
                    Err(SanitizeError::ValueOutOfBounds)
                } else {
                    shred.sanitize()
                }
            }
            CrdsData::SnapshotHashes(val) => val.sanitize(),
            CrdsData::ContactInfo(node) => node.sanitize(),
            CrdsData::RestartLastVotedForkSlots(slots) => slots.sanitize(),
            CrdsData::RestartHeaviestFork(fork) => fork.sanitize(),
            // Deprecated
            CrdsData::AccountsHashes(_)
            | CrdsData::Vote(_)
            | CrdsData::LowestSlot(_)
            | CrdsData::EpochSlots(_)
            | CrdsData::LegacySnapshotHashes(_)
            | CrdsData::LegacyContactInfo(_)
            | CrdsData::LegacyVersion(_)
            | CrdsData::NodeInstance(_)
            | CrdsData::Version(_) => Err(SanitizeError::InvalidValue),
        }
    }
}

/// Random timestamp for tests and benchmarks.
pub(crate) fn new_rand_timestamp<R: Rng>(rng: &mut R) -> u64 {
    const DELAY: u64 = 10 * 60 * 1000; // 10 minutes
    timestamp() - DELAY + rng.random_range(0..2 * DELAY)
}

impl CrdsData {
    /// New random CrdsData for tests and benchmarks.
    pub(crate) fn new_rand<R: Rng>(rng: &mut R, pubkey: Option<Pubkey>) -> CrdsData {
        let kind = rng.random_range(0..4);
        // TODO: Implement other kinds of CrdsData here.
        // TODO: Assign ranges to each arm proportional to their frequency in
        // the mainnet crds table.
        match kind {
            0 => CrdsData::from(ContactInfo::new_rand(rng, pubkey)),
            1 => CrdsData::RestartLastVotedForkSlots(RestartLastVotedForkSlots::new_rand(
                rng, pubkey,
            )),
            2 => CrdsData::RestartHeaviestFork(RestartHeaviestFork::new_rand(rng, pubkey)),
            _ => CrdsData::SnapshotHashes(SnapshotHashes {
                from: pubkey.unwrap_or_else(solana_pubkey::new_rand),
                full: (rng.random_range(0..MAX_SLOT), Hash::new_unique()),
                incremental: Vec::new(),
                wallclock: new_rand_timestamp(rng),
            }),
        }
    }

    pub(crate) fn wallclock(&self) -> u64 {
        match self {
            CrdsData::DuplicateShred(_, shred) => shred.wallclock,
            CrdsData::SnapshotHashes(hash) => hash.wallclock,
            CrdsData::ContactInfo(node) => node.wallclock(),
            CrdsData::RestartLastVotedForkSlots(slots) => slots.wallclock,
            CrdsData::RestartHeaviestFork(fork) => fork.wallclock,
            // Deprecated: sanitize() rejects these before any caller reaches here.
            CrdsData::AccountsHashes(_)
            | CrdsData::Vote(_)
            | CrdsData::LowestSlot(_)
            | CrdsData::EpochSlots(_)
            | CrdsData::LegacySnapshotHashes(_)
            | CrdsData::LegacyContactInfo(_)
            | CrdsData::LegacyVersion(_)
            | CrdsData::NodeInstance(_)
            | CrdsData::Version(_) => unreachable!("deprecated CrdsData variant"),
        }
    }

    pub(crate) fn pubkey(&self) -> Pubkey {
        match &self {
            CrdsData::DuplicateShred(_, shred) => shred.from,
            CrdsData::SnapshotHashes(hash) => hash.from,
            CrdsData::ContactInfo(node) => *node.pubkey(),
            CrdsData::RestartLastVotedForkSlots(slots) => slots.from,
            CrdsData::RestartHeaviestFork(fork) => fork.from,
            // Deprecated: sanitize() rejects these before any caller reaches here.
            CrdsData::AccountsHashes(_)
            | CrdsData::Vote(_)
            | CrdsData::LowestSlot(_)
            | CrdsData::EpochSlots(_)
            | CrdsData::LegacySnapshotHashes(_)
            | CrdsData::LegacyContactInfo(_)
            | CrdsData::LegacyVersion(_)
            | CrdsData::NodeInstance(_)
            | CrdsData::Version(_) => unreachable!("deprecated CrdsData variant"),
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn is_deprecated(&self) -> bool {
        match self {
            Self::LegacyContactInfo(_) => true,
            Self::Vote(_) => true,
            Self::LowestSlot(_) => true,
            Self::LegacySnapshotHashes(_) => true,
            Self::AccountsHashes(_) => true,
            Self::EpochSlots(_) => true,
            Self::LegacyVersion(_) => true,
            Self::Version(_) => true,
            Self::NodeInstance(_) => true,
            Self::DuplicateShred(..) => false,
            Self::SnapshotHashes(_) => false,
            Self::ContactInfo(_) => false,
            Self::RestartLastVotedForkSlots(_) => true,
            Self::RestartHeaviestFork(_) => true,
        }
    }
}

impl From<ContactInfo> for CrdsData {
    #[inline]
    fn from(node: ContactInfo) -> Self {
        Self::ContactInfo(node)
    }
}

impl From<&ContactInfo> for CrdsData {
    #[inline]
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

macro_rules! reject_deserialize {
    ($ty:ty, $msg:expr) => {
        // Deprecated types fail to deserialize, whether read directly or as a
        // CrdsData variant.
        unsafe impl<'de, C: wincode::config::Config> wincode::SchemaRead<'de, C> for $ty {
            type Dst = Self;
            fn read(
                _reader: impl wincode::io::Reader<'de>,
                _dst: &mut std::mem::MaybeUninit<Self>,
            ) -> wincode::ReadResult<()> {
                Err(wincode::ReadError::Custom($msg))
            }
        }
    };
}
pub(crate) use reject_deserialize;

// `CrdsData` derives only `StableAbi` (not `StableAbiSample`): the auto-derived
// enum sampler picks a uniform variant including the deprecated ones, which wrap
// `Deprecated` and reject deserialization. This sampler emits only the
// non-deprecated, round-trippable variants, so the digest test can use
// `test_roundtrip = "eq_and_wire"`.
#[cfg(feature = "stable-abi")]
impl solana_frozen_abi::rand::distr::Distribution<CrdsData>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> CrdsData {
        use solana_frozen_abi::stable_abi::StableAbi;
        match rng.random_range(0..5u8) {
            0 => CrdsData::DuplicateShred(rng.random(), <DuplicateShred as StableAbi>::random(rng)),
            1 => CrdsData::SnapshotHashes(<SnapshotHashes as StableAbi>::random(rng)),
            2 => CrdsData::ContactInfo(<ContactInfo as StableAbi>::random(rng)),
            3 => CrdsData::RestartLastVotedForkSlots(
                <RestartLastVotedForkSlots as StableAbi>::random(rng),
            ),
            _ => CrdsData::RestartHeaviestFork(<RestartHeaviestFork as StableAbi>::random(rng)),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_deprecated_values_fail_deserialization() {
        let deprecated_values = [
            CrdsData::Vote(Deprecated {}),
            CrdsData::LowestSlot(Deprecated {}),
            CrdsData::EpochSlots(Deprecated {}),
            CrdsData::NodeInstance(Deprecated {}),
            CrdsData::LegacyVersion(Deprecated {}),
            CrdsData::Version(Deprecated {}),
            CrdsData::LegacyContactInfo(Deprecated {}),
            CrdsData::AccountsHashes(Deprecated {}),
            CrdsData::LegacySnapshotHashes(Deprecated {}),
        ];

        for value in &deprecated_values {
            assert!(value.is_deprecated());
            assert_eq!(value.sanitize(), Err(SanitizeError::InvalidValue));
            let bytes = wincode::serialize(value).unwrap();
            assert!(wincode::deserialize::<CrdsData>(&bytes[..]).is_err());
        }
    }
}
