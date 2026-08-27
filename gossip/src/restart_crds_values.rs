use {
    crate::crds_data::new_rand_timestamp,
    bv::BitVec,
    itertools::Itertools,
    rand::Rng,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    solana_wincode_varint::Leb128Int,
    thiserror::Error,
    wincode::{SchemaRead, SchemaWrite},
};

#[cfg_attr(
    feature = "frozen-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "3mueGPQfnZCXQfb5bdTU1afXeFToiJxpFGkckSSjH3RD",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct RestartLastVotedForkSlots {
    pub from: Pubkey,
    pub wallclock: u64,
    offsets: SlotsOffsets,
    pub last_voted_slot: Slot,
    pub last_voted_hash: Hash,
    pub shred_version: u16,
}

#[derive(Debug, Error)]
pub enum RestartLastVotedForkSlotsError {
    #[error("Last voted fork cannot be empty")]
    LastVotedForkEmpty,
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "9uCfdqWTWRZoxarLXEJ2HcJgvNs65jfFYK4fBReMkJzJ",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
// repr(C) makes this struct zero-copy eligible in wincode.
#[repr(C)]
pub struct RestartHeaviestFork {
    pub from: Pubkey,
    pub wallclock: u64,
    pub last_slot: Slot,
    pub last_slot_hash: Hash,
    pub observed_stake: u64,
    pub shred_version: u16,
}

#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
enum SlotsOffsets {
    RunLengthEncoding(RunLengthEncoding),
    RawOffsets(RawOffsets),
}

#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
struct U16(#[wincode(with = "Leb128Int<u16>")] u16);

// The vector always starts with 1. Encode number of 1's and 0's consecutively.
// For example, 110000111 is [2, 4, 3].
#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
struct RunLengthEncoding(Vec<U16>);

#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
struct RawOffsets(
    // `BitVec` has no `StableAbi` impl; sample a non-empty one (see `sample_bit_vec`).
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "solana_bloom::bloom::sample_bit_vec(rng)")
    )]
    BitVec<u8>,
);

impl Sanitize for RestartLastVotedForkSlots {
    fn sanitize(&self) -> std::result::Result<(), SanitizeError> {
        Err(SanitizeError::InvalidValue)
    }
}

impl RestartLastVotedForkSlots {
    // This number is MAX_CRDS_OBJECT_SIZE - empty serialized RestartLastVotedForkSlots.
    const MAX_BYTES: usize = 824;

    // Per design doc, we should start wen_restart within 7 hours.
    pub const MAX_SLOTS: usize = u16::MAX as usize;

    pub fn new(
        from: Pubkey,
        now: u64,
        last_voted_fork: &[Slot],
        last_voted_hash: Hash,
        shred_version: u16,
    ) -> Result<Self, RestartLastVotedForkSlotsError> {
        let Some((&first_voted_slot, &last_voted_slot)) =
            last_voted_fork.iter().minmax().into_option()
        else {
            return Err(RestartLastVotedForkSlotsError::LastVotedForkEmpty);
        };
        let max_size = last_voted_slot.saturating_sub(first_voted_slot) + 1;
        let mut uncompressed_bitvec = BitVec::new_fill(false, max_size);
        for slot in last_voted_fork {
            uncompressed_bitvec.set(last_voted_slot - *slot, true);
        }
        let run_length_encoding = RunLengthEncoding::new(&uncompressed_bitvec);
        let offsets =
            if run_length_encoding.num_encoded_slots() > RestartLastVotedForkSlots::MAX_BYTES * 8 {
                SlotsOffsets::RunLengthEncoding(run_length_encoding)
            } else {
                SlotsOffsets::RawOffsets(RawOffsets::new(uncompressed_bitvec))
            };
        Ok(Self {
            from,
            wallclock: now,
            offsets,
            last_voted_slot,
            last_voted_hash,
            shred_version,
        })
    }

    /// New random Version for tests and benchmarks.
    pub(crate) fn new_rand<R: Rng>(rng: &mut R, pubkey: Option<Pubkey>) -> Self {
        let pubkey = pubkey.unwrap_or_else(solana_pubkey::new_rand);
        let num_slots = rng.random_range(2..20);
        let slots = std::iter::repeat_with(|| 47825632 + rng.random_range(0..512))
            .take(num_slots)
            .collect::<Vec<Slot>>();
        RestartLastVotedForkSlots::new(
            pubkey,
            new_rand_timestamp(rng),
            &slots,
            Hash::new_unique(),
            1,
        )
        .unwrap()
    }

    pub fn to_slots(&self, min_slot: Slot) -> Vec<Slot> {
        match &self.offsets {
            SlotsOffsets::RunLengthEncoding(run_length_encoding) => {
                run_length_encoding.to_slots(self.last_voted_slot, min_slot)
            }
            SlotsOffsets::RawOffsets(raw_offsets) => {
                raw_offsets.to_slots(self.last_voted_slot, min_slot)
            }
        }
    }
}

impl Sanitize for RestartHeaviestFork {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        Err(SanitizeError::InvalidValue)
    }
}

impl RestartHeaviestFork {
    pub(crate) fn new_rand<R: Rng>(rng: &mut R, from: Option<Pubkey>) -> Self {
        let from = from.unwrap_or_else(solana_pubkey::new_rand);
        Self {
            from,
            wallclock: new_rand_timestamp(rng),
            last_slot: rng.random_range(0..1000),
            last_slot_hash: Hash::new_unique(),
            observed_stake: rng.random_range(1..u64::MAX),
            shred_version: 1,
        }
    }
}

impl RunLengthEncoding {
    fn new(bits: &BitVec<u8>) -> Self {
        let encoded = (0..bits.len())
            .map(|i| bits.get(i))
            .dedup_with_count()
            .map_while(|(count, _)| u16::try_from(count).ok())
            .scan(0, |current_bytes, count| {
                *current_bytes += (u16::BITS - count.leading_zeros()).div_ceil(7).max(1) as usize;
                (*current_bytes <= RestartLastVotedForkSlots::MAX_BYTES).then_some(U16(count))
            })
            .collect();
        Self(encoded)
    }

    fn num_encoded_slots(&self) -> usize {
        self.0.iter().map(|x| usize::from(x.0)).sum()
    }

    fn to_slots(&self, last_slot: Slot, min_slot: Slot) -> Vec<Slot> {
        let mut slots: Vec<Slot> = self
            .0
            .iter()
            .map(|bit_count| usize::from(bit_count.0))
            .zip([1, 0].iter().cycle())
            .flat_map(|(bit_count, bit)| std::iter::repeat_n(bit, bit_count))
            .enumerate()
            .filter(|(_, bit)| **bit == 1)
            .map_while(|(offset, _)| {
                let offset = Slot::try_from(offset).ok()?;
                last_slot.checked_sub(offset)
            })
            .take(RestartLastVotedForkSlots::MAX_SLOTS)
            .take_while(|slot| *slot >= min_slot)
            .collect();
        slots.reverse();
        slots
    }
}

impl RawOffsets {
    fn new(mut bits: BitVec<u8>) -> Self {
        bits.truncate(RestartLastVotedForkSlots::MAX_BYTES as u64 * 8);
        bits.shrink_to_fit();
        Self(bits)
    }

    fn to_slots(&self, last_slot: Slot, min_slot: Slot) -> Vec<Slot> {
        let mut slots: Vec<Slot> = (0..self.0.len())
            .filter(|index| self.0.get(*index))
            .map_while(|offset| last_slot.checked_sub(offset))
            .take_while(|slot| *slot >= min_slot)
            .collect();
        slots.reverse();
        slots
    }
}
