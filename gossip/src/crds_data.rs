#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
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
    solana_transaction::Transaction,
    solana_vote::vote_parser,
    std::mem::MaybeUninit,
    wincode::{ReadError, ReadResult, SchemaRead, SchemaWrite, config::Config, io::Reader},
};

pub(crate) const MAX_WALLCLOCK: u64 = 1_000_000_000_000_000;
pub(crate) const MAX_SLOT: u64 = 1_000_000_000_000_000;

pub(crate) type VoteIndex = u8;
// Until the cluster upgrades we allow votes from higher indices
const OLD_MAX_VOTES: VoteIndex = 32;
/// Number of votes per validator to store.
pub const MAX_VOTES: VoteIndex = 12;

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
        abi_digest = "FbbLp1LrFoE5L6kywwvB2kL4kFtyPm2fVGmAUb6KJtZF",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[cfg_attr(test, derive(strum_macros::EnumCount))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
pub enum CrdsData {
    #[allow(private_interfaces)]
    LegacyContactInfo(Deprecated), // Deprecated
    Vote(VoteIndex, Vote),
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
            CrdsData::Vote(ix, val) => {
                if *ix >= OLD_MAX_VOTES {
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
            CrdsData::LowestSlot(_)
            | CrdsData::EpochSlots(_)
            | CrdsData::AccountsHashes(_)
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
            1 => CrdsData::SnapshotHashes(SnapshotHashes {
                from: pubkey.unwrap_or_else(solana_pubkey::new_rand),
                full: (0, Hash::new_from_array(rng.random())),
                incremental: Vec::new(),
                wallclock: new_rand_timestamp(rng),
            }),
            2 => CrdsData::Vote(rng.random_range(0..MAX_VOTES), Vote::new_rand(rng, pubkey)),
            3 => CrdsData::RestartLastVotedForkSlots(RestartLastVotedForkSlots::new_rand(
                rng, pubkey,
            )),
            _ => CrdsData::RestartHeaviestFork(RestartHeaviestFork::new_rand(rng, pubkey)),
        }
    }

    pub(crate) fn wallclock(&self) -> u64 {
        match self {
            CrdsData::Vote(_, vote) => vote.wallclock,
            CrdsData::DuplicateShred(_, shred) => shred.wallclock,
            CrdsData::SnapshotHashes(hash) => hash.wallclock,
            CrdsData::ContactInfo(node) => node.wallclock(),
            CrdsData::RestartLastVotedForkSlots(slots) => slots.wallclock,
            CrdsData::RestartHeaviestFork(fork) => fork.wallclock,
            // Deprecated: sanitize() rejects these before any caller reaches here.
            CrdsData::LowestSlot(_)
            | CrdsData::EpochSlots(_)
            | CrdsData::AccountsHashes(_)
            | CrdsData::LegacySnapshotHashes(_)
            | CrdsData::LegacyContactInfo(_)
            | CrdsData::LegacyVersion(_)
            | CrdsData::NodeInstance(_)
            | CrdsData::Version(_) => unreachable!("deprecated CrdsData variant"),
        }
    }

    pub(crate) fn pubkey(&self) -> Pubkey {
        match &self {
            CrdsData::Vote(_, vote) => vote.from,
            CrdsData::DuplicateShred(_, shred) => shred.from,
            CrdsData::SnapshotHashes(hash) => hash.from,
            CrdsData::ContactInfo(node) => *node.pubkey(),
            CrdsData::RestartLastVotedForkSlots(slots) => slots.from,
            CrdsData::RestartHeaviestFork(fork) => fork.from,
            // Deprecated: sanitize() rejects these before any caller reaches here.
            CrdsData::LowestSlot(_)
            | CrdsData::EpochSlots(_)
            | CrdsData::AccountsHashes(_)
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
            Self::Vote(..) => false,
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

#[cfg_attr(
    feature = "stable-abi",
    derive(StableAbi),
    frozen_abi(
        abi_digest = "BuDZ4shsJhBqELG47eMS3cDM8EcqHBGMSVu99JeW6VdM",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite)]
pub struct Vote {
    pub(crate) from: Pubkey,
    transaction: Transaction,
    pub(crate) wallclock: u64,
    #[wincode(skip)]
    slot: Option<Slot>,
}

impl Sanitize for Vote {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        sanitize_wallclock(self.wallclock)?;
        self.from.sanitize()?;
        self.transaction.sanitize()
    }
}

impl Vote {
    // Returns None if cannot parse transaction into a vote.
    pub fn new(from: Pubkey, transaction: Transaction, wallclock: u64) -> Option<Self> {
        vote_parser::parse_vote_transaction(&transaction).map(|(_, vote, ..)| Self {
            from,
            transaction,
            wallclock,
            slot: vote.last_voted_slot(),
        })
    }

    /// New random Vote for tests and benchmarks.
    fn new_rand<R: Rng>(rng: &mut R, pubkey: Option<Pubkey>) -> Self {
        Self {
            from: pubkey.unwrap_or_else(solana_pubkey::new_rand),
            transaction: Transaction::default(),
            wallclock: new_rand_timestamp(rng),
            slot: None,
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn transaction(&self) -> &Transaction {
        &self.transaction
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn from(&self) -> &Pubkey {
        &self.from
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn wallclock(&self) -> u64 {
        self.wallclock
    }

    pub(crate) fn slot(&self) -> Option<Slot> {
        self.slot
    }
}

unsafe impl<'de, C: Config> SchemaRead<'de, C> for Vote {
    type Dst = Self;

    fn read(reader: impl Reader<'de>, dst: &mut MaybeUninit<Self>) -> ReadResult<()> {
        #[derive(SchemaRead)]
        struct VoteLite {
            from: Pubkey,
            transaction: Transaction,
            wallclock: u64,
        }
        let lite = <VoteLite as SchemaRead<'de, C>>::get(reader)?;
        lite.transaction
            .sanitize()
            .map_err(|_| ReadError::Custom("Vote: invalid transaction"))?;
        let vote = Vote::new(lite.from, lite.transaction, lite.wallclock)
            .ok_or(ReadError::Custom("Vote: invalid vote tx"))?;
        dst.write(vote);
        Ok(())
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

// `Vote` derives only `StableAbi` (not `StableAbiSample`): its `slot` field is
// recomputed from the transaction on deserialize, and the transaction must parse
// as a vote, so independent per-field sampling can't produce round-trippable
// values. Building via `Vote::new` with a valid vote transaction does, which lets
// the digest test use `test_roundtrip = "eq_and_wire"`.
#[cfg(feature = "stable-abi")]
impl solana_frozen_abi::rand::distr::Distribution<Vote>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> Vote {
        use {solana_frozen_abi::stable_abi::StableAbi, solana_keypair::Keypair};
        let node = Keypair::new_from_array(rng.random());
        let vote = Keypair::new_from_array(rng.random());
        let authorized_voter = Keypair::new_from_array(rng.random());
        let transaction = solana_vote::vote_transaction::new_vote_transaction(
            /*slots:*/ vec![rng.random()],
            /*bank_hash:*/ <Hash as StableAbi>::random(rng),
            /*blockhash:*/ <Hash as StableAbi>::random(rng),
            &node,
            &vote,
            &authorized_voter,
            /*switch_proof_hash:*/ None,
        );
        Vote::new(
            <Pubkey as StableAbi>::random(rng),
            transaction,
            rng.random(),
        )
        .expect("vote transaction must parse as a vote")
    }
}

// `CrdsData` derives only `StableAbi` (not `StableAbiSample`): the auto-derived
// enum sampler picks a uniform variant including the deprecated ones, which wrap
// `Deprecated` and reject deserialization. This sampler emits only the
// round-trippable variants, so the digest test can use
// `test_roundtrip = "eq_and_wire"`.
#[cfg(feature = "stable-abi")]
impl solana_frozen_abi::rand::distr::Distribution<CrdsData>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> CrdsData {
        use solana_frozen_abi::stable_abi::StableAbi;
        match rng.random_range(0..6u8) {
            0 => CrdsData::Vote(rng.random(), <Vote as StableAbi>::random(rng)),
            1 => CrdsData::DuplicateShred(rng.random(), <DuplicateShred as StableAbi>::random(rng)),
            2 => CrdsData::SnapshotHashes(<SnapshotHashes as StableAbi>::random(rng)),
            3 => CrdsData::ContactInfo(<ContactInfo as StableAbi>::random(rng)),
            4 => CrdsData::RestartLastVotedForkSlots(
                <RestartLastVotedForkSlots as StableAbi>::random(rng),
            ),
            _ => CrdsData::RestartHeaviestFork(<RestartHeaviestFork as StableAbi>::random(rng)),
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::crds_value::CrdsValue,
        solana_keypair::Keypair,
        solana_perf::test_tx::new_test_vote_tx,
        solana_signer::Signer,
        solana_time_utils::timestamp,
        solana_vote_program::{vote_instruction, vote_state},
    };

    #[test]
    fn test_max_vote_index() {
        let mut rng = rand::rng();
        let keypair = Keypair::new();
        let vote = Vote::new(keypair.pubkey(), new_test_vote_tx(&mut rng), timestamp()).unwrap();
        let vote = CrdsValue::new(CrdsData::Vote(OLD_MAX_VOTES, vote), &keypair);
        assert!(vote.sanitize().is_err());
    }

    #[test]
    fn test_vote_round_trip() {
        let mut rng = rand::rng();
        let vote = vote_state::Vote::new(
            vec![1, 3, 7], // slots
            Hash::new_unique(),
        );
        let ix = vote_instruction::vote(
            &Pubkey::new_unique(), // vote_pubkey
            &Pubkey::new_unique(), // authorized_voter_pubkey
            vote,
        );
        let tx = Transaction::new_with_payer(
            &[ix],                       // instructions
            Some(&Pubkey::new_unique()), // payer
        );
        let vote = Vote::new(
            Pubkey::new_unique(), // from
            tx,
            rng.random(), // wallclock
        )
        .unwrap();
        assert_eq!(vote.slot, Some(7));
        let bytes = wincode::serialize(&vote).unwrap();
        let other: Vote = wincode::deserialize(&bytes[..]).unwrap();
        assert_eq!(vote, other);
        assert_eq!(other.slot, Some(7));
    }

    #[test]
    fn test_lowest_slot_rejected() {
        // Legacy LowestSlot: discriminant, index, from, root, lowest, slots, stash, wallclock.
        for index in [0u8, 1, u8::MAX] {
            let bytes = wincode::serialize(&(
                2u32,
                index,
                Pubkey::new_unique(),
                0u64,
                42u64,
                Vec::<Slot>::new(),
                Vec::<u8>::new(),
                timestamp(),
            ))
            .unwrap();
            assert!(wincode::deserialize::<CrdsData>(&bytes).is_err());
        }
        let value = CrdsData::LowestSlot(Deprecated {});
        assert!(value.is_deprecated());
        assert_eq!(value.sanitize(), Err(SanitizeError::InvalidValue));
    }

    #[test]
    fn test_epoch_slots_rejected() {
        // Historic EpochSlots: discriminant, index, from, slots, wallclock.
        let bytes = wincode::serialize(&(
            5u32,
            0u8,
            Pubkey::new_unique(),
            Vec::<u8>::new(),
            timestamp(),
        ))
        .unwrap();
        assert!(wincode::deserialize::<CrdsData>(&bytes).is_err());
        let value = CrdsData::EpochSlots(Deprecated {});
        assert!(value.is_deprecated());
        assert_eq!(value.sanitize(), Err(SanitizeError::InvalidValue));
    }

    #[test]
    fn test_deprecated_values_fail_deserialization() {
        let deprecated_values = [
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
            let bytes = wincode::serialize(value).unwrap();
            assert!(wincode::deserialize::<CrdsData>(&bytes[..]).is_err());
        }
    }
}
