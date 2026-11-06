#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample, frozen_abi};
use {
    crate::{
        contact_info::ContactInfo,
        duplicate_shred::{DuplicateShred, DuplicateShredIndex, MAX_DUPLICATE_SHREDS},
        epoch_slots::EpochSlots,
        restart_crds_values::{RestartHeaviestFork, RestartLastVotedForkSlots},
    },
    rand::Rng,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::{self, Pubkey},
    solana_sanitize::{Sanitize, SanitizeError},
    solana_time_utils::timestamp,
    std::{collections::BTreeSet, mem::MaybeUninit},
    wincode::{
        ReadError, ReadResult, SchemaRead, SchemaWrite, TypeMeta, WriteResult,
        config::Config,
        io::{Reader, Writer},
    },
};

pub(crate) const MAX_WALLCLOCK: u64 = 1_000_000_000_000_000;
pub(crate) const MAX_SLOT: u64 = 1_000_000_000_000_000;

pub(crate) type EpochSlotsIndex = u8;
pub(crate) const MAX_EPOCH_SLOTS: EpochSlotsIndex = 255;

// Helper for deprecated types
#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite)]
pub(crate) struct Deprecated {}
reject_deserialize!(Deprecated, "Trying to deserialize deprecated type");

// Wincode schema that reads a u8 and rejects non-zero values, used on the
// deprecated LowestSlot index field.
struct RejectNonzeroU8;
unsafe impl<C: Config> SchemaWrite<C> for RejectNonzeroU8 {
    type Src = u8;
    const TYPE_META: TypeMeta = <u8 as SchemaWrite<C>>::TYPE_META;

    fn size_of(src: &u8) -> WriteResult<usize> {
        <u8 as SchemaWrite<C>>::size_of(src)
    }
    fn write(writer: impl Writer, src: &u8) -> WriteResult<()> {
        <u8 as SchemaWrite<C>>::write(writer, src)
    }
}
unsafe impl<'de, C: Config> SchemaRead<'de, C> for RejectNonzeroU8 {
    type Dst = u8;
    const TYPE_META: TypeMeta = <u8 as SchemaRead<'de, C>>::TYPE_META.keep_zero_copy(false);

    fn read(reader: impl Reader<'de>, dst: &mut MaybeUninit<u8>) -> ReadResult<()> {
        let index = <u8 as SchemaRead<'de, C>>::get(reader)?;
        if index != 0 {
            return Err(ReadError::Custom("LowestSlot index must be 0"));
        }
        dst.write(index);
        Ok(())
    }
}

/// CrdsData that defines the different types of items CrdsValues can hold
/// * Merge Strategy - Latest wallclock is picked
/// * LowestSlot index is deprecated
#[allow(clippy::large_enum_variant)]
#[cfg_attr(
    feature = "stable-abi",
    derive(StableAbi),
    frozen_abi(
        abi_digest = "8xtXp6zcdy4CUu2fpyEcKTWSDrq4KvGLRcBTv73ojnL7",
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
    LowestSlot(
        #[wincode(with = "RejectNonzeroU8")] u8, // u8 is deprecated
        LowestSlot,
    ),
    #[allow(private_interfaces)]
    LegacySnapshotHashes(Deprecated), // Deprecated
    #[allow(private_interfaces)]
    AccountsHashes(Deprecated), // Deprecated
    EpochSlots(EpochSlotsIndex, EpochSlots),
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
            CrdsData::LowestSlot(ix, val) => {
                if *ix as usize >= 1 {
                    return Err(SanitizeError::ValueOutOfBounds);
                }
                val.sanitize()
            }
            CrdsData::EpochSlots(ix, val) => {
                if *ix as usize >= MAX_EPOCH_SLOTS as usize {
                    return Err(SanitizeError::ValueOutOfBounds);
                }
                val.sanitize()
            }
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
        let kind = rng.random_range(0..5);
        // TODO: Implement other kinds of CrdsData here.
        // TODO: Assign ranges to each arm proportional to their frequency in
        // the mainnet crds table.
        match kind {
            0 => CrdsData::from(ContactInfo::new_rand(rng, pubkey)),
            // Index for LowestSlot is deprecated and should be zero.
            1 => CrdsData::LowestSlot(0, LowestSlot::new_rand(rng, pubkey)),
            2 => CrdsData::RestartLastVotedForkSlots(RestartLastVotedForkSlots::new_rand(
                rng, pubkey,
            )),
            3 => CrdsData::RestartHeaviestFork(RestartHeaviestFork::new_rand(rng, pubkey)),
            _ => CrdsData::EpochSlots(
                rng.random_range(0..MAX_EPOCH_SLOTS),
                EpochSlots::new_rand(rng, pubkey),
            ),
        }
    }

    pub(crate) fn wallclock(&self) -> u64 {
        match self {
            CrdsData::LowestSlot(_, obj) => obj.wallclock,
            CrdsData::EpochSlots(_, p) => p.wallclock,
            CrdsData::DuplicateShred(_, shred) => shred.wallclock,
            CrdsData::SnapshotHashes(hash) => hash.wallclock,
            CrdsData::ContactInfo(node) => node.wallclock(),
            CrdsData::RestartLastVotedForkSlots(slots) => slots.wallclock,
            CrdsData::RestartHeaviestFork(fork) => fork.wallclock,
            // Deprecated: sanitize() rejects these before any caller reaches here.
            CrdsData::AccountsHashes(_)
            | CrdsData::Vote(_)
            | CrdsData::LegacySnapshotHashes(_)
            | CrdsData::LegacyContactInfo(_)
            | CrdsData::LegacyVersion(_)
            | CrdsData::NodeInstance(_)
            | CrdsData::Version(_) => unreachable!("deprecated CrdsData variant"),
        }
    }

    pub(crate) fn pubkey(&self) -> Pubkey {
        match &self {
            CrdsData::LowestSlot(_, slots) => slots.from,
            CrdsData::EpochSlots(_, p) => p.from,
            CrdsData::DuplicateShred(_, shred) => shred.from,
            CrdsData::SnapshotHashes(hash) => hash.from,
            CrdsData::ContactInfo(node) => *node.pubkey(),
            CrdsData::RestartLastVotedForkSlots(slots) => slots.from,
            CrdsData::RestartHeaviestFork(fork) => fork.from,
            // Deprecated: sanitize() rejects these before any caller reaches here.
            CrdsData::AccountsHashes(_)
            | CrdsData::Vote(_)
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
            Self::LowestSlot(0, _) => false,
            Self::LowestSlot(1.., _) => true,
            Self::LegacySnapshotHashes(_) => true,
            Self::AccountsHashes(_) => true,
            Self::EpochSlots(..) => false,
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

#[cfg_attr(
    feature = "stable-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "BjFXktfmwd6TT8kdWVEeMJquARTUe8cUvo2oDwzMfhPi",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct LowestSlot {
    pub(crate) from: Pubkey,
    root: Slot, //deprecated
    pub lowest: Slot,
    slots: BTreeSet<Slot>, //deprecated
    stash: Vec<u8>,        //deprecated
    wallclock: u64,
}

impl LowestSlot {
    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn wallclock(&self) -> u64 {
        self.wallclock
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn from(&self) -> &Pubkey {
        &self.from
    }

    pub fn new(from: Pubkey, lowest: Slot, wallclock: u64) -> Self {
        Self {
            from,
            root: 0,
            lowest,
            slots: BTreeSet::new(),
            stash: vec![],
            wallclock,
        }
    }

    /// New random LowestSlot for tests and benchmarks.
    fn new_rand<R: Rng>(rng: &mut R, pubkey: Option<Pubkey>) -> Self {
        Self {
            from: pubkey.unwrap_or_else(solana_pubkey::new_rand),
            root: rng.random(),
            lowest: rng.random(),
            slots: BTreeSet::default(),
            stash: Vec::default(),
            wallclock: new_rand_timestamp(rng),
        }
    }
}

impl Sanitize for LowestSlot {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        sanitize_wallclock(self.wallclock)?;
        if self.lowest >= MAX_SLOT {
            return Err(SanitizeError::ValueOutOfBounds);
        }
        if self.root != 0 {
            return Err(SanitizeError::InvalidValue);
        }
        if !self.slots.is_empty() {
            return Err(SanitizeError::InvalidValue);
        }
        if !self.stash.is_empty() {
            return Err(SanitizeError::InvalidValue);
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
// non-deprecated, round-trippable variants (with `LowestSlot`'s index pinned to
// 0), so the digest test can use `test_roundtrip = "eq_and_wire"`.
#[cfg(feature = "stable-abi")]
impl solana_frozen_abi::rand::distr::Distribution<CrdsData>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> CrdsData {
        use solana_frozen_abi::stable_abi::StableAbi;
        match rng.random_range(0..7u8) {
            0 => CrdsData::LowestSlot(0, <LowestSlot as StableAbi>::random(rng)),
            1 => CrdsData::EpochSlots(rng.random(), <EpochSlots as StableAbi>::random(rng)),
            2 => CrdsData::DuplicateShred(rng.random(), <DuplicateShred as StableAbi>::random(rng)),
            3 => CrdsData::SnapshotHashes(<SnapshotHashes as StableAbi>::random(rng)),
            4 => CrdsData::ContactInfo(<ContactInfo as StableAbi>::random(rng)),
            5 => CrdsData::RestartLastVotedForkSlots(
                <RestartLastVotedForkSlots as StableAbi>::random(rng),
            ),
            _ => CrdsData::RestartHeaviestFork(<RestartHeaviestFork as StableAbi>::random(rng)),
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*, crate::crds_value::CrdsValue, solana_keypair::Keypair, solana_signer::Signer,
        solana_time_utils::timestamp,
    };

    #[test]
    fn test_lowest_slot_sanitize() {
        let ls = LowestSlot::new(Pubkey::default(), 0, 0);
        let v = CrdsValue::new_unsigned(CrdsData::LowestSlot(0, ls.clone()));
        assert_eq!(v.sanitize(), Ok(()));

        let mut o = ls.clone();
        o.root = 1;
        let v = CrdsValue::new_unsigned(CrdsData::LowestSlot(0, o));
        assert_eq!(v.sanitize(), Err(SanitizeError::InvalidValue));

        let o = ls.clone();
        let v = CrdsValue::new_unsigned(CrdsData::LowestSlot(1, o));
        assert_eq!(v.sanitize(), Err(SanitizeError::ValueOutOfBounds));

        let mut o = ls.clone();
        o.slots.insert(1);
        let v = CrdsValue::new_unsigned(CrdsData::LowestSlot(0, o));
        assert_eq!(v.sanitize(), Err(SanitizeError::InvalidValue));

        let mut o = ls;
        o.stash.push(0);
        let v = CrdsValue::new_unsigned(CrdsData::LowestSlot(0, o));
        assert_eq!(v.sanitize(), Err(SanitizeError::InvalidValue));
    }

    #[test]
    fn test_max_epoch_slots_index() {
        let keypair = Keypair::new();
        let item = CrdsValue::new(
            CrdsData::EpochSlots(
                MAX_EPOCH_SLOTS,
                EpochSlots::new(keypair.pubkey(), timestamp()),
            ),
            &keypair,
        );
        assert_eq!(item.sanitize(), Err(SanitizeError::ValueOutOfBounds));
    }

    #[test]
    fn test_deprecated_values_fail_deserialization() {
        let deprecated_values = [
            CrdsData::Vote(Deprecated {}),
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

        let keypair = Keypair::new();

        // LowestSlot(1, ...)
        let lowest_slot =
            CrdsData::LowestSlot(1, LowestSlot::new(keypair.pubkey(), 0, timestamp()));
        let bytes = wincode::serialize(&lowest_slot).unwrap();
        assert!(wincode::deserialize::<CrdsData>(&bytes[..]).is_err());

        // LowestSlot(0, ...) -> should be deserialized successfully
        let lowest_slot =
            CrdsData::LowestSlot(0, LowestSlot::new(keypair.pubkey(), 0, timestamp()));
        let bytes = wincode::serialize(&lowest_slot).unwrap();
        assert!(wincode::deserialize::<CrdsData>(&bytes[..]).is_ok());
    }
}
