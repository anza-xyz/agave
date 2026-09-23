#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::crds_data::sanitize_wallclock,
    bytes::Bytes,
    itertools::Itertools,
    solana_clock::Slot,
    solana_ledger::{
        blockstore::BlockstoreError,
        blockstore_meta::DuplicateSlotProof,
        shred::{
            Admissible, AdmissionPolicy, AnyShred, ParseError, RejectReason, Shred, ShredType,
            parse_turbine,
        },
    },
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    std::{
        collections::{HashMap, hash_map::Entry},
        convert::TryFrom,
        num::TryFromIntError,
    },
    thiserror::Error,
    wincode::{ReadError, SchemaRead, SchemaWrite, WriteError},
};

const DUPLICATE_SHRED_HEADER_SIZE: usize = 63;

pub(crate) type DuplicateShredIndex = u16;
pub(crate) const MAX_DUPLICATE_SHREDS: DuplicateShredIndex = 512;

#[cfg_attr(
    feature = "frozen-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "9zVjcmgLcLv1YDhBwDFYgMMjtpoZjk77YoL3sLBnABTf",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
pub struct DuplicateShred {
    pub(crate) from: Pubkey,
    pub(crate) wallclock: u64,
    pub(crate) slot: Slot,
    _unused: u32,
    // NOTE: This field was previously typed as `ShredType`.
    // It is semantically unused, so we now deserialize it as a plain `u8`
    // to avoid strict enum validation errors on bad data.
    _unused_shred_type: u8,
    // Serialized DuplicateSlotProof split into chunks.
    num_chunks: u8,
    chunk_index: u8,
    chunk: Vec<u8>,
}

impl DuplicateShred {
    #[inline]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn num_chunks(&self) -> u8 {
        self.num_chunks
    }

    #[inline]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn chunk_index(&self) -> u8 {
        self.chunk_index
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    #[inline]
    pub(crate) fn from(&self) -> &Pubkey {
        &self.from
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    #[inline]
    pub(crate) fn wallclock(&self) -> u64 {
        self.wallclock
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    #[inline]
    pub(crate) fn slot(&self) -> Slot {
        self.slot
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    #[inline]
    pub(crate) fn chunk(&self) -> &[u8] {
        &self.chunk
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("block store save error")]
    BlockstoreInsertFailed(#[from] BlockstoreError),
    #[error("data chunk mismatch")]
    DataChunkMismatch,
    #[error("unable to send duplicate slot to state machine")]
    DuplicateSlotSenderFailure,
    #[error("invalid chunk_index: {chunk_index}, num_chunks: {num_chunks}")]
    InvalidChunkIndex { chunk_index: u8, num_chunks: u8 },
    #[error("invalid duplicate shreds")]
    InvalidDuplicateShreds,
    #[error("invalid duplicate slot proof")]
    InvalidDuplicateSlotProof,
    #[error("invalid erasure meta conflict")]
    InvalidErasureMetaConflict,
    #[error("invalid last index conflict")]
    InvalidLastIndexConflict,
    #[error("invalid shred version: {0}")]
    InvalidShredVersion(u16),
    #[error("invalid signature")]
    InvalidSignature,
    #[error("invalid size limit")]
    InvalidSizeLimit,
    #[error(transparent)]
    InvalidShred(#[from] ParseError),
    #[error(transparent)]
    RejectedShred(#[from] RejectReason),
    #[error("number of chunks mismatch")]
    NumChunksMismatch,
    #[error("missing data chunk")]
    MissingDataChunk,
    #[error("wincode deserialization error")]
    WincodeReadError(#[from] ReadError),
    #[error("wincode serialization error")]
    WincodeWriteError(#[from] WriteError),
    #[error("shred type mismatch")]
    ShredTypeMismatch,
    #[error("slot mismatch")]
    SlotMismatch,
    #[error("type conversion error")]
    TryFromIntError(#[from] TryFromIntError),
    #[error("unknown slot leader: {0}")]
    UnknownSlotLeader(Slot),
}

impl Error {
    /// Errors indicating that the initial node submitted an invalid duplicate proof case
    pub(crate) fn is_non_critical(&self) -> bool {
        match self {
            Self::SlotMismatch
            | Self::InvalidShredVersion(_)
            | Self::InvalidSignature
            | Self::ShredTypeMismatch
            | Self::InvalidDuplicateShreds
            | Self::InvalidLastIndexConflict
            | Self::InvalidErasureMetaConflict
            | Self::RejectedShred(_) => true,
            Self::BlockstoreInsertFailed(_)
            | Self::DataChunkMismatch
            | Self::DuplicateSlotSenderFailure
            | Self::InvalidChunkIndex { .. }
            | Self::InvalidDuplicateSlotProof
            | Self::InvalidSizeLimit
            | Self::InvalidShred(_)
            | Self::NumChunksMismatch
            | Self::MissingDataChunk
            | Self::WincodeReadError(_)
            | Self::WincodeWriteError(_)
            | Self::TryFromIntError(_)
            | Self::UnknownSlotLeader(_) => false,
        }
    }
}

fn parse_proof_shred(bytes: Bytes) -> Result<AnyShred<Admissible>, Error> {
    let shred = parse_turbine(bytes)?;
    let policy = AdmissionPolicy {
        shred_version: shred.version(),
        root: 0,
        max_slot: Slot::MAX,
        max_data_shreds_per_slot: u32::MAX,
        max_code_shreds_per_slot: u32::MAX,
    };
    Ok(shred.check_policy(&policy)?)
}

fn verify_proof_shred(shred: AnyShred<Admissible>, leader: &Pubkey) -> Result<Shred, Error> {
    shred.verify(leader).map_err(|_| Error::InvalidSignature)
}

/// Check that `shred1` and `shred2` indicate a valid duplicate proof
///     - Must be for the same slot
///     - Must match the expected shred version
///     - Must have a merkle root conflict, otherwise `shred1` and `shred2` must have the same `shred_type`
///     - If `shred1` and `shred2` share the same index they must be not have equal payloads excluding the
///       retransmitter signature
///     - If `shred1` and `shred2` do not share the same index and are data shreds
///       verify that they indicate an index conflict. One of them must be the
///       LAST_SHRED_IN_SLOT, however the other shred must have a higher index.
///     - If `shred1` and `shred2` do not share the same index and are coding shreds
///       verify that they have conflicting erasure metas
fn check_shreds(shred1: &Shred, shred2: &Shred, shred_version: u16) -> Result<(), Error> {
    if shred1.slot() != shred2.slot() {
        return Err(Error::SlotMismatch);
    }

    if shred1.version() != shred_version {
        return Err(Error::InvalidShredVersion(shred1.version()));
    }
    if shred2.version() != shred_version {
        return Err(Error::InvalidShredVersion(shred2.version()));
    }

    if shred1.fec_set_index() == shred2.fec_set_index()
        && shred1.merkle_root().ok() != shred2.merkle_root().ok()
    {
        return Ok(());
    }

    if shred1.kind() != shred2.kind() {
        return Err(Error::ShredTypeMismatch);
    }

    if shred1.index() == shred2.index() {
        if shred1.is_duplicate_of(shred2) {
            return Ok(());
        }
        return Err(Error::InvalidDuplicateShreds);
    }

    if shred1.kind() == ShredType::Data {
        if shred1.last_in_slot() && shred2.index() > shred1.index() {
            return Ok(());
        }
        if shred2.last_in_slot() && shred1.index() > shred2.index() {
            return Ok(());
        }
        return Err(Error::InvalidLastIndexConflict);
    }

    // This mirrors the current logic in blockstore to detect coding shreds with conflicting
    // erasure sets. However this is not technically exhaustive, as any 2 shreds with
    // different but overlapping erasure sets can be considered duplicate and need not be
    // a part of the same fec set. Further work to enhance detection is planned in
    // https://github.com/solana-labs/solana/issues/33037
    if shred1.fec_set_index() == shred2.fec_set_index()
        && shred1.erasure_mismatch(shred2) == Some(true)
    {
        return Ok(());
    }
    Err(Error::InvalidErasureMetaConflict)
}

fn chunk_proof(
    proof: &DuplicateSlotProof,
    slot: Slot,
    self_pubkey: Pubkey,
    wallclock: u64,
    max_size: usize,
) -> Result<impl Iterator<Item = DuplicateShred> + use<>, Error> {
    let data = wincode::serialize(proof)?;
    let chunk_size = if DUPLICATE_SHRED_HEADER_SIZE < max_size {
        max_size - DUPLICATE_SHRED_HEADER_SIZE
    } else {
        return Err(Error::InvalidSizeLimit);
    };
    let chunks: Vec<_> = data.chunks(chunk_size).map(Vec::from).collect();
    let num_chunks = u8::try_from(chunks.len())?;
    let chunks = chunks
        .into_iter()
        .enumerate()
        .map(move |(i, chunk)| DuplicateShred {
            from: self_pubkey,
            wallclock,
            slot,
            num_chunks,
            chunk_index: i as u8,
            chunk,
            _unused: 0,
            _unused_shred_type: ShredType::Code.into(),
        });
    Ok(chunks)
}

pub(crate) fn from_shred<F>(
    shred: Shred,
    self_pubkey: Pubkey, // Pubkey of my node broadcasting crds value.
    other_payload: Bytes,
    leader_schedule: Option<F>,
    wallclock: u64,
    max_size: usize, // Maximum serialized size of each DuplicateShred.
    shred_version: u16,
) -> Result<impl Iterator<Item = DuplicateShred>, Error>
where
    F: FnOnce(Slot) -> Option<Pubkey>,
{
    if *shred.bytes() == other_payload {
        return Err(Error::InvalidDuplicateShreds);
    }
    let other_shred = match leader_schedule {
        Some(leader_schedule) => {
            let other_shred = parse_proof_shred(other_payload)?;
            if shred.slot() != other_shred.slot() {
                return Err(Error::SlotMismatch);
            }
            let slot_leader =
                leader_schedule(shred.slot()).ok_or(Error::UnknownSlotLeader(shred.slot()))?;
            verify_proof_shred(other_shred, &slot_leader)?
        }
        None => Shred::from_blockstore(other_payload)?,
    };
    check_shreds(&shred, &other_shred, shred_version)?;
    let slot = shred.slot();
    let proof = DuplicateSlotProof {
        shred1: shred.into_bytes(),
        shred2: other_shred.into_bytes(),
    };
    chunk_proof(&proof, slot, self_pubkey, wallclock, max_size)
}

// Returns a predicate checking if a duplicate-shred chunk matches
// the slot and has valid chunk_index.
fn check_chunk(slot: Slot, num_chunks: u8) -> impl Fn(&DuplicateShred) -> Result<(), Error> {
    move |dup| {
        if dup.slot != slot {
            Err(Error::SlotMismatch)
        } else if dup.num_chunks != num_chunks {
            Err(Error::NumChunksMismatch)
        } else if dup.chunk_index >= num_chunks {
            Err(Error::InvalidChunkIndex {
                chunk_index: dup.chunk_index,
                num_chunks,
            })
        } else {
            Ok(())
        }
    }
}

/// Reconstructs the duplicate shreds from chunks of DuplicateShred.
pub(crate) fn into_shreds(
    slot_leader: &Pubkey,
    chunks: impl IntoIterator<Item = DuplicateShred>,
    shred_version: u16,
) -> Result<(Shred, Shred), Error> {
    let mut chunks = chunks.into_iter();
    let DuplicateShred {
        slot,
        num_chunks,
        chunk_index,
        chunk,
        ..
    } = chunks.next().ok_or(Error::InvalidDuplicateShreds)?;
    let check_chunk = check_chunk(slot, num_chunks);
    let mut data = HashMap::new();
    data.insert(chunk_index, chunk);
    for chunk in chunks {
        check_chunk(&chunk)?;
        match data.entry(chunk.chunk_index) {
            Entry::Vacant(entry) => {
                entry.insert(chunk.chunk);
            }
            Entry::Occupied(entry) => {
                if *entry.get() != chunk.chunk {
                    return Err(Error::DataChunkMismatch);
                }
            }
        }
    }
    if data.len() != num_chunks as usize {
        return Err(Error::MissingDataChunk);
    }
    let data = (0..num_chunks).map(|k| data.remove(&k).unwrap()).concat();
    let proof: DuplicateSlotProof = wincode::deserialize(&data)?;
    if proof.shred1 == proof.shred2 {
        return Err(Error::InvalidDuplicateSlotProof);
    }
    let shred1 = parse_proof_shred(proof.shred1)?;
    let shred2 = parse_proof_shred(proof.shred2)?;

    if shred1.slot() != slot || shred2.slot() != slot {
        return Err(Error::SlotMismatch);
    }
    if shred1.version() != shred_version {
        return Err(Error::InvalidShredVersion(shred1.version()));
    }
    if shred2.version() != shred_version {
        return Err(Error::InvalidShredVersion(shred2.version()));
    }
    let shred1 = verify_proof_shred(shred1, slot_leader)?;
    let shred2 = verify_proof_shred(shred2, slot_leader)?;

    check_shreds(&shred1, &shred2, shred_version)?;
    Ok((shred1, shred2))
}

impl Sanitize for DuplicateShred {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        sanitize_wallclock(self.wallclock)?;
        if self.chunk_index >= self.num_chunks {
            return Err(SanitizeError::IndexOutOfBounds);
        }
        self.from.sanitize()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        rand::Rng,
        solana_entry::entry::Entry,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::shred::{ProcessShredsStats, Shredder},
        solana_signature::Signature,
        solana_signer::Signer,
        solana_system_transaction::transfer,
        std::sync::Arc,
    };

    #[test]
    fn test_duplicate_shred_header_size() {
        let dup = DuplicateShred {
            from: Pubkey::new_unique(),
            wallclock: u64::MAX,
            slot: Slot::MAX,
            _unused_shred_type: ShredType::Data.into(),
            num_chunks: u8::MAX,
            chunk_index: u8::MAX,
            chunk: Vec::default(),
            _unused: 0,
        };
        let dup_bytes = wincode::serialize(&dup).unwrap();
        assert_eq!(dup_bytes.len(), DUPLICATE_SHRED_HEADER_SIZE);
        let dup_size = wincode::serialized_size(&dup).unwrap();
        assert_eq!(dup_size, DUPLICATE_SHRED_HEADER_SIZE as u64);
    }

    pub(crate) fn rand_fec_set_index<R: Rng>(rng: &mut R) -> u32 {
        rng.random_range(0..1_000) * 32
    }

    pub(crate) fn new_rand_shred<R: Rng>(
        rng: &mut R,
        next_shred_index: u32,
        shredder: &Shredder,
        keypair: &Keypair,
    ) -> Shred {
        let (mut data_shreds, _) =
            new_rand_shreds(rng, next_shred_index, 5, shredder, keypair, true);
        data_shreds.pop().unwrap()
    }

    fn new_rand_data_shred<R: Rng>(
        rng: &mut R,
        next_shred_index: u32,
        shredder: &Shredder,
        keypair: &Keypair,
        is_last_in_slot: bool,
    ) -> Shred {
        let (mut data_shreds, _) =
            new_rand_shreds(rng, next_shred_index, 5, shredder, keypair, is_last_in_slot);
        data_shreds.pop().unwrap()
    }

    fn new_rand_coding_shreds<R: Rng>(
        rng: &mut R,
        next_shred_index: u32,
        num_entries: usize,
        shredder: &Shredder,
        keypair: &Keypair,
    ) -> Vec<Shred> {
        let (_, coding_shreds) =
            new_rand_shreds(rng, next_shred_index, num_entries, shredder, keypair, true);
        coding_shreds
    }

    fn new_rand_shreds<R: Rng>(
        rng: &mut R,
        next_shred_index: u32,
        num_entries: usize,
        shredder: &Shredder,
        keypair: &Keypair,
        is_last_in_slot: bool,
    ) -> (Vec<Shred>, Vec<Shred>) {
        let entries: Vec<_> = std::iter::repeat_with(|| {
            let tx = transfer(
                &Keypair::new(),       // from
                &Pubkey::new_unique(), // to
                rng.random(),          // lamports
                Hash::new_unique(),    // recent blockhash
            );
            Entry::new(
                &Hash::new_unique(), // prev_hash
                1,                   // num_hashes,
                vec![tx],            // transactions
            )
        })
        .take(num_entries)
        .collect();
        shredder.entries_to_merkle_shreds_for_tests(
            keypair,
            &entries,
            is_last_in_slot,
            // chained_merkle_root
            Hash::new_from_array(rng.random()),
            next_shred_index,
            &mut ProcessShredsStats::default(),
        )
    }

    fn from_shred_bypass_checks(
        shred: Shred,
        self_pubkey: Pubkey, // Pubkey of my node broadcasting crds value.
        other_shred: Shred,
        wallclock: u64,
        max_size: usize, // Maximum serialized size of each DuplicateShred.
    ) -> Result<impl Iterator<Item = DuplicateShred>, Error> {
        let slot = shred.slot();
        let proof = DuplicateSlotProof {
            shred1: shred.into_bytes(),
            shred2: other_shred.into_bytes(),
        };
        chunk_proof(&proof, slot, self_pubkey, wallclock, max_size)
    }

    #[test]
    fn test_duplicate_shred_round_trip() {
        let mut rng = rand::rng();
        let leader = Arc::new(Keypair::new());
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = rand_fec_set_index(&mut rng);
        let shred1 = new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, true);
        let shred2 = new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, true);
        let leader_schedule = |s| {
            if s == slot {
                Some(leader.pubkey())
            } else {
                None
            }
        };
        let chunks: Vec<_> = from_shred(
            shred1.clone(),
            Pubkey::new_unique(), // self_pubkey
            shred2.bytes().clone(),
            Some(leader_schedule),
            rng.random(), // wallclock
            512,          // max_size
            version,
        )
        .unwrap()
        .collect();
        assert!(chunks.len() > 4);
        let (shred3, shred4) = into_shreds(&leader.pubkey(), chunks, version).unwrap();
        assert_eq!(shred1, shred3);
        assert_eq!(shred2, shred4);
    }

    #[test]
    fn test_duplicate_shred_invalid() {
        let mut rng = rand::rng();
        let leader = Arc::new(Keypair::new());
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = rand_fec_set_index(&mut rng);
        let leader_schedule = |s| {
            if s == slot {
                Some(leader.pubkey())
            } else {
                None
            }
        };
        let data_shred = new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, true);
        let coding_shreds =
            new_rand_coding_shreds(&mut rng, next_shred_index, 10, &shredder, &leader);
        let test_cases = vec![
            // Same data_shred
            (data_shred.clone(), data_shred),
            // Same coding_shred
            (coding_shreds[0].clone(), coding_shreds[0].clone()),
        ];
        for (shred1, shred2) in test_cases.into_iter() {
            assert_matches!(
                from_shred(
                    shred1.clone(),
                    Pubkey::new_unique(), // self_pubkey
                    shred2.bytes().clone(),
                    Some(leader_schedule),
                    rng.random(), // wallclock
                    512,          // max_size
                    version,
                )
                .err()
                .unwrap(),
                Error::InvalidDuplicateShreds
            );

            let chunks: Vec<_> = from_shred_bypass_checks(
                shred1.clone(),
                Pubkey::new_unique(), // self_pubkey
                shred2.clone(),
                rng.random(), // wallclock
                512,          // max_size
            )
            .unwrap()
            .collect();
            assert!(chunks.len() > 4);

            assert_matches!(
                into_shreds(&leader.pubkey(), chunks, version)
                    .err()
                    .unwrap(),
                Error::InvalidDuplicateSlotProof
            );
        }
    }

    #[test]
    fn test_latest_index_conflict_round_trip() {
        let mut rng = rand::rng();
        let leader = Arc::new(Keypair::new());
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = rand_fec_set_index(&mut rng);
        let leader_schedule = |s| {
            if s == slot {
                Some(leader.pubkey())
            } else {
                None
            }
        };
        let test_cases = [
            (
                new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, true),
                new_rand_data_shred(&mut rng, next_shred_index + 32, &shredder, &leader, false),
            ),
            (
                new_rand_data_shred(&mut rng, next_shred_index + 128, &shredder, &leader, true),
                new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, true),
            ),
        ];
        for (shred1, shred2) in test_cases.iter().flat_map(|(a, b)| [(a, b), (b, a)]) {
            let chunks: Vec<_> = from_shred(
                shred1.clone(),
                Pubkey::new_unique(), // self_pubkey
                shred2.bytes().clone(),
                Some(leader_schedule),
                rng.random(), // wallclock
                512,          // max_size
                version,
            )
            .unwrap()
            .collect();
            assert!(chunks.len() > 4);
            let (shred3, shred4) = into_shreds(&leader.pubkey(), chunks, version).unwrap();
            assert_eq!(shred1, &shred3);
            assert_eq!(shred2, &shred4);
        }
    }

    #[test]
    fn test_latest_index_conflict_invalid() {
        let mut rng = rand::rng();
        let leader = Arc::new(Keypair::new());
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = rand_fec_set_index(&mut rng);
        let leader_schedule = |s| {
            if s == slot {
                Some(leader.pubkey())
            } else {
                None
            }
        };
        let test_cases = vec![
            (
                new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, false),
                new_rand_data_shred(&mut rng, next_shred_index + 32, &shredder, &leader, true),
            ),
            (
                new_rand_data_shred(&mut rng, next_shred_index + 32, &shredder, &leader, true),
                new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, false),
            ),
            (
                new_rand_data_shred(&mut rng, next_shred_index + 128, &shredder, &leader, false),
                new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, false),
            ),
            (
                new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, false),
                new_rand_data_shred(&mut rng, next_shred_index + 128, &shredder, &leader, false),
            ),
        ];
        for (shred1, shred2) in test_cases.into_iter() {
            assert_matches!(
                from_shred(
                    shred1.clone(),
                    Pubkey::new_unique(), // self_pubkey
                    shred2.bytes().clone(),
                    Some(leader_schedule),
                    rng.random(), // wallclock
                    512,          // max_size
                    version,
                )
                .err()
                .unwrap(),
                Error::InvalidLastIndexConflict
            );

            let chunks: Vec<_> = from_shred_bypass_checks(
                shred1.clone(),
                Pubkey::new_unique(), // self_pubkey
                shred2.clone(),
                rng.random(), // wallclock
                512,          // max_size
            )
            .unwrap()
            .collect();
            assert!(chunks.len() > 4);

            assert_matches!(
                into_shreds(&leader.pubkey(), chunks, version)
                    .err()
                    .unwrap(),
                Error::InvalidLastIndexConflict
            );
        }
    }

    #[test]
    fn test_erasure_meta_conflict_invalid() {
        let mut rng = rand::rng();
        let leader = Arc::new(Keypair::new());
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = rand_fec_set_index(&mut rng);
        let leader_schedule = |s| {
            if s == slot {
                Some(leader.pubkey())
            } else {
                None
            }
        };
        let coding_shreds =
            new_rand_coding_shreds(&mut rng, next_shred_index, 10, &shredder, &leader);
        let coding_shreds_different_fec =
            new_rand_coding_shreds(&mut rng, next_shred_index + 32, 10, &shredder, &leader);

        let test_cases = vec![
            // Different index, different fec set, same erasure meta
            (
                coding_shreds[0].clone(),
                coding_shreds_different_fec[1].clone(),
            ),
            // Different index, same fec set, same erasure meta
            (coding_shreds[0].clone(), coding_shreds[1].clone()),
            (
                coding_shreds_different_fec[0].clone(),
                coding_shreds_different_fec[1].clone(),
            ),
        ];
        for (shred1, shred2) in test_cases.into_iter() {
            assert_matches!(
                from_shred(
                    shred1.clone(),
                    Pubkey::new_unique(), // self_pubkey
                    shred2.bytes().clone(),
                    Some(leader_schedule),
                    rng.random(), // wallclock
                    512,          // max_size
                    version,
                )
                .err()
                .unwrap(),
                Error::InvalidErasureMetaConflict
            );

            let chunks: Vec<_> = from_shred_bypass_checks(
                shred1.clone(),
                Pubkey::new_unique(), // self_pubkey
                shred2.clone(),
                rng.random(), // wallclock
                512,          // max_size
            )
            .unwrap()
            .collect();
            assert!(chunks.len() > 4);

            assert_matches!(
                into_shreds(&leader.pubkey(), chunks, version)
                    .err()
                    .unwrap(),
                Error::InvalidErasureMetaConflict
            );
        }
    }

    #[test]
    fn test_merkle_root_conflict_round_trip() {
        let mut rng = rand::rng();
        let leader = Arc::new(Keypair::new());
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = rand_fec_set_index(&mut rng);
        let leader_schedule = |s| {
            if s == slot {
                Some(leader.pubkey())
            } else {
                None
            }
        };

        let (data_shreds, coding_shreds) =
            new_rand_shreds(&mut rng, next_shred_index, 10, &shredder, &leader, false);

        let (diff_data_shreds, diff_coding_shreds) =
            new_rand_shreds(&mut rng, next_shred_index, 10, &shredder, &leader, false);

        let test_cases = vec![
            (data_shreds[0].clone(), diff_data_shreds[1].clone()),
            (coding_shreds[0].clone(), diff_coding_shreds[1].clone()),
            (data_shreds[0].clone(), diff_coding_shreds[0].clone()),
            (coding_shreds[0].clone(), diff_data_shreds[0].clone()),
        ];
        for (shred1, shred2) in test_cases.into_iter() {
            let chunks: Vec<_> = from_shred(
                shred1.clone(),
                Pubkey::new_unique(), // self_pubkey
                shred2.bytes().clone(),
                Some(leader_schedule),
                rng.random(), // wallclock
                512,          // max_size
                version,
            )
            .unwrap()
            .collect();
            assert!(chunks.len() > 4);
            let (shred3, shred4) = into_shreds(&leader.pubkey(), chunks, version).unwrap();
            assert_eq!(shred1, shred3);
            assert_eq!(shred2, shred4);
        }
    }

    #[test]
    fn test_merkle_root_conflict_invalid() {
        let mut rng = rand::rng();
        let leader = Arc::new(Keypair::new());
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = rand_fec_set_index(&mut rng);
        let leader_schedule = |s| {
            if s == slot {
                Some(leader.pubkey())
            } else {
                None
            }
        };

        let (data_shreds, coding_shreds) =
            new_rand_shreds(&mut rng, next_shred_index, 10, &shredder, &leader, true);

        let (next_data_shreds, next_coding_shreds) = new_rand_shreds(
            &mut rng,
            next_shred_index + 32,
            10,
            &shredder,
            &leader,
            true,
        );

        let test_cases = vec![
            // Same fec set same merkle root
            (coding_shreds[0].clone(), data_shreds[0].clone()),
            (data_shreds[0].clone(), coding_shreds[0].clone()),
            // Different FEC set different merkle root
            (coding_shreds[0].clone(), next_data_shreds[0].clone()),
            (next_coding_shreds[0].clone(), data_shreds[0].clone()),
            (data_shreds[0].clone(), next_coding_shreds[0].clone()),
            (next_data_shreds[0].clone(), coding_shreds[0].clone()),
        ];
        for (shred1, shred2) in test_cases.into_iter() {
            assert_matches!(
                from_shred(
                    shred1.clone(),
                    Pubkey::new_unique(), // self_pubkey
                    shred2.bytes().clone(),
                    Some(leader_schedule),
                    rng.random(), // wallclock
                    512,          // max_size
                    version,
                )
                .err()
                .unwrap(),
                Error::ShredTypeMismatch
            );

            let chunks: Vec<_> = from_shred_bypass_checks(
                shred1.clone(),
                Pubkey::new_unique(), // self_pubkey
                shred2.clone(),
                rng.random(), // wallclock
                512,          // max_size
            )
            .unwrap()
            .collect();
            assert!(chunks.len() > 4);

            assert_matches!(
                into_shreds(&leader.pubkey(), chunks, version)
                    .err()
                    .unwrap(),
                Error::ShredTypeMismatch
            );
        }
    }

    #[test]
    fn test_shred_version() {
        let mut rng = rand::rng();
        let leader = Arc::new(Keypair::new());
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = rand_fec_set_index(&mut rng);
        let leader_schedule = |s| {
            if s == slot {
                Some(leader.pubkey())
            } else {
                None
            }
        };

        let (data_shreds, coding_shreds) =
            new_rand_shreds(&mut rng, next_shred_index, 10, &shredder, &leader, true);

        // Wrong shred version 1
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version + 1).unwrap();
        let (wrong_data_shreds_1, wrong_coding_shreds_1) =
            new_rand_shreds(&mut rng, next_shred_index, 10, &shredder, &leader, true);

        // Wrong shred version 2
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version + 2).unwrap();
        let (wrong_data_shreds_2, wrong_coding_shreds_2) =
            new_rand_shreds(&mut rng, next_shred_index, 10, &shredder, &leader, true);

        let test_cases = vec![
            // One correct shred version, one wrong
            (coding_shreds[0].clone(), wrong_coding_shreds_1[0].clone()),
            (coding_shreds[0].clone(), wrong_data_shreds_1[0].clone()),
            (data_shreds[0].clone(), wrong_coding_shreds_1[0].clone()),
            (data_shreds[0].clone(), wrong_data_shreds_1[0].clone()),
            // Both wrong shred version
            (
                wrong_coding_shreds_2[0].clone(),
                wrong_coding_shreds_1[0].clone(),
            ),
            (
                wrong_coding_shreds_2[0].clone(),
                wrong_data_shreds_1[0].clone(),
            ),
            (
                wrong_data_shreds_2[0].clone(),
                wrong_coding_shreds_1[0].clone(),
            ),
            (
                wrong_data_shreds_2[0].clone(),
                wrong_data_shreds_1[0].clone(),
            ),
        ];

        for (shred1, shred2) in test_cases.into_iter() {
            assert_matches!(
                from_shred(
                    shred1.clone(),
                    Pubkey::new_unique(), // self_pubkey
                    shred2.bytes().clone(),
                    Some(leader_schedule),
                    rng.random(), // wallclock
                    512,          // max_size
                    version,
                )
                .err()
                .unwrap(),
                Error::InvalidShredVersion(_)
            );

            let chunks: Vec<_> = from_shred_bypass_checks(
                shred1.clone(),
                Pubkey::new_unique(), // self_pubkey
                shred2.clone(),
                rng.random(), // wallclock
                512,          // max_size
            )
            .unwrap()
            .collect();
            assert!(chunks.len() > 4);
            assert_matches!(
                into_shreds(&leader.pubkey(), chunks, version)
                    .err()
                    .unwrap(),
                Error::InvalidShredVersion(_)
            );
        }
    }

    fn with_random_retransmitter_signature(shred: &Shred, leader: &Pubkey) -> Shred {
        assert!(
            shred.variant().resigned(),
            "only last-FEC-set shreds carry a retransmitter signature"
        );
        let mut bytes = shred.bytes().to_vec();
        let offset = bytes.len() - 64;
        bytes[offset..].copy_from_slice(Signature::new_unique().as_ref());
        let shred = parse_proof_shred(Bytes::from(bytes)).unwrap();
        shred.verify(leader).unwrap()
    }

    #[test]
    fn test_retransmitter_signature_invalid() {
        let mut rng = rand::rng();
        let leader = Arc::new(Keypair::new());
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = rand_fec_set_index(&mut rng);
        let leader_schedule = |s| {
            if s == slot {
                Some(leader.pubkey())
            } else {
                None
            }
        };
        let data_shred = new_rand_data_shred(&mut rng, next_shred_index, &shredder, &leader, true);
        let coding_shred =
            new_rand_coding_shreds(&mut rng, next_shred_index, 10, &shredder, &leader)[0].clone();
        let data_shred_different_retransmitter =
            with_random_retransmitter_signature(&data_shred, &leader.pubkey());
        let coding_shred_different_retransmitter =
            with_random_retransmitter_signature(&coding_shred, &leader.pubkey());

        let test_cases = [
            (data_shred, data_shred_different_retransmitter),
            // Same coding shred from different retransmitter
            (coding_shred, coding_shred_different_retransmitter),
        ];
        for (shred1, shred2) in test_cases.iter().flat_map(|(a, b)| [(a, b), (b, a)]) {
            assert_matches!(
                from_shred(
                    shred1.clone(),
                    Pubkey::new_unique(), // self_pubkey
                    shred2.bytes().clone(),
                    Some(leader_schedule),
                    rng.random(), // wallclock
                    512,          // max_size
                    version,
                )
                .err()
                .unwrap(),
                Error::InvalidDuplicateShreds
            );

            let chunks: Vec<_> = from_shred_bypass_checks(
                shred1.clone(),
                Pubkey::new_unique(), // self_pubkey
                shred2.clone(),
                rng.random(), // wallclock
                512,          // max_size
            )
            .unwrap()
            .collect();
            assert!(chunks.len() > 4);

            assert_matches!(
                into_shreds(&leader.pubkey(), chunks, version)
                    .err()
                    .unwrap(),
                Error::InvalidDuplicateShreds
            );
        }
    }
}
