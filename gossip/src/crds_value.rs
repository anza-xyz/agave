use {
    crate::{
        contact_info::ContactInfo,
        crds_data::{CrdsData, EpochSlotsIndex, VoteIndex},
        duplicate_shred::DuplicateShredIndex,
        epoch_slots::EpochSlots,
    },
    bincode::Options,
    itertools::Either,
    rand::Rng,
    solana_sanitize::{Sanitize, SanitizeError},
    solana_sdk::{
        hash::Hash,
        packet::Encode,
        pubkey::Pubkey,
        signature::{Keypair, Signable, Signature, Signer, SIGNATURE_BYTES},
    },
    std::{
        borrow::{Borrow, Cow},
        io::{Cursor, Error as IoError, ErrorKind as IoErrorKind, Write},
    },
};

/// CrdsValue that is replicated across the cluster
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrdsValue {
    signature: Signature,
    data: CrdsData,
    hash: Hash, // Sha256 hash of [signature, data].
    // Bincode serialized self.data.
    bincode_serialized_data: Vec<u8>,
}

impl Sanitize for CrdsValue {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        self.signature.sanitize()?;
        self.data.sanitize()
    }
}

impl Signable for CrdsValue {
    fn pubkey(&self) -> Pubkey {
        self.pubkey()
    }

    fn signable_data(&self) -> Cow<[u8]> {
        Cow::Borrowed(&self.bincode_serialized_data)
    }

    fn get_signature(&self) -> Signature {
        self.signature
    }

    fn set_signature(&mut self, signature: Signature) {
        self.signature = signature
    }

    fn verify(&self) -> bool {
        self.get_signature()
            .verify(self.pubkey().as_ref(), self.signable_data().borrow())
    }
}

/// Type of the replicated value
/// These are labels for values in a record that is associated with `Pubkey`
#[derive(PartialEq, Hash, Eq, Clone, Debug)]
pub enum CrdsValueLabel {
    LegacyContactInfo(Pubkey),
    Vote(VoteIndex, Pubkey),
    LowestSlot(Pubkey),
    LegacySnapshotHashes(Pubkey),
    EpochSlots(EpochSlotsIndex, Pubkey),
    AccountsHashes(Pubkey),
    LegacyVersion(Pubkey),
    Version(Pubkey),
    NodeInstance(Pubkey),
    DuplicateShred(DuplicateShredIndex, Pubkey),
    SnapshotHashes(Pubkey),
    ContactInfo(Pubkey),
    RestartLastVotedForkSlots(Pubkey),
    RestartHeaviestFork(Pubkey),
}

impl CrdsValueLabel {
    pub fn pubkey(&self) -> Pubkey {
        match self {
            CrdsValueLabel::LegacyContactInfo(p) => *p,
            CrdsValueLabel::Vote(_, p) => *p,
            CrdsValueLabel::LowestSlot(p) => *p,
            CrdsValueLabel::LegacySnapshotHashes(p) => *p,
            CrdsValueLabel::EpochSlots(_, p) => *p,
            CrdsValueLabel::AccountsHashes(p) => *p,
            CrdsValueLabel::LegacyVersion(p) => *p,
            CrdsValueLabel::Version(p) => *p,
            CrdsValueLabel::NodeInstance(p) => *p,
            CrdsValueLabel::DuplicateShred(_, p) => *p,
            CrdsValueLabel::SnapshotHashes(p) => *p,
            CrdsValueLabel::ContactInfo(pubkey) => *pubkey,
            CrdsValueLabel::RestartLastVotedForkSlots(p) => *p,
            CrdsValueLabel::RestartHeaviestFork(p) => *p,
        }
    }
}

impl CrdsValue {
    pub fn new(data: CrdsData, keypair: &Keypair) -> Self {
        let bincode_serialized_data = bincode::serialize(&data).unwrap();
        let signature = keypair.sign_message(&bincode_serialized_data);
        let hash = solana_sdk::hash::hashv(&[signature.as_ref(), &bincode_serialized_data]);
        Self {
            signature,
            data,
            hash,
            bincode_serialized_data,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_unsigned(data: CrdsData) -> Self {
        let bincode_serialized_data = bincode::serialize(&data).unwrap();
        let signature = Signature::default();
        let hash = solana_sdk::hash::hashv(&[signature.as_ref(), &bincode_serialized_data]);
        Self {
            signature,
            data,
            hash,
            bincode_serialized_data,
        }
    }

    /// New random CrdsValue for tests and benchmarks.
    pub fn new_rand<R: Rng>(rng: &mut R, keypair: Option<&Keypair>) -> CrdsValue {
        match keypair {
            None => {
                let keypair = Keypair::new();
                let data = CrdsData::new_rand(rng, Some(keypair.pubkey()));
                Self::new(data, &keypair)
            }
            Some(keypair) => {
                let data = CrdsData::new_rand(rng, Some(keypair.pubkey()));
                Self::new(data, keypair)
            }
        }
    }

    #[inline]
    pub(crate) fn signature(&self) -> &Signature {
        &self.signature
    }

    #[inline]
    pub(crate) fn data(&self) -> &CrdsData {
        &self.data
    }

    #[inline]
    pub(crate) fn hash(&self) -> &Hash {
        &self.hash
    }

    /// Totally unsecure unverifiable wallclock of the node that generated this message
    /// Latest wallclock is always picked.
    /// This is used to time out push messages.
    pub(crate) fn wallclock(&self) -> u64 {
        self.data.wallclock()
    }

    pub(crate) fn pubkey(&self) -> Pubkey {
        self.data.pubkey()
    }

    // Implements bincode::serialize_into for CrdsValue.
    pub(crate) fn bincode_serialize<W: Write>(&self, writer: &mut W) -> Result<(), IoError> {
        writer.write_all(self.signature.as_ref())?;
        writer.write_all(&self.bincode_serialized_data)
    }

    // Implements bincode::serialize_into for Vec<CrdsValue>.
    pub(crate) fn bincode_serialize_many<I, T: Borrow<Self>, W: Write>(
        values: I,
        writer: &mut W,
    ) -> Result<(), IoError>
    where
        I: IntoIterator<Item = T>,
        <I as IntoIterator>::IntoIter: ExactSizeIterator,
    {
        let mut values = values.into_iter();
        let size = u64::try_from(values.len()).unwrap();
        writer.write_all(&size.to_le_bytes())?;
        values.try_for_each(|value| value.borrow().bincode_serialize(writer))
    }

    // Implements bincode::deserialize_from for CrdsValue.
    pub(crate) fn bincode_deserialize(
        bytes: &[u8],
        allow_trailing_bytes: bool,
    ) -> Result<Self, bincode::Error> {
        let (signature, bytes) = convert_fixed_bytes::<Signature, SIGNATURE_BYTES>(bytes)?;
        let mut cursor = Cursor::new(bytes);
        // Default bincode options:
        //  * unlimited byte limit
        //  * little endian
        //  * varint encoding
        //  * rejects trailing bytes
        // https://docs.rs/bincode/1.3.3/bincode/fn.options.html
        let options = bincode::options()
            .with_limit(bytes.len() as u64)
            .with_fixint_encoding();
        let data: CrdsData = if allow_trailing_bytes {
            options.allow_trailing_bytes().deserialize_from(&mut cursor)
        } else {
            options.deserialize_from(&mut cursor).and_then(|data| {
                // We have to manually check if all bytes are read because
                // options.reject_trailing_bytes() does not really work.
                // https://github.com/bincode-org/bincode/issues/732
                if cursor.position() == cursor.get_ref().len() as u64 {
                    Ok(data)
                } else {
                    Err(bincode::Error::from(bincode::ErrorKind::Custom(
                        String::from("Slice had bytes remaining after deserialization"),
                    )))
                }
            })
        }?;
        let offset = usize::try_from(cursor.position()).map_err(|err| {
            let err = format!("{err:?}, cursor: {}", cursor.position());
            bincode::ErrorKind::Custom(err)
        })?;
        let bincode_serialized_data = bytes.get(..offset).map(Vec::from).ok_or_else(|| {
            let err = format!("Invalid offset: {offset}, bytes.len(): {}", bytes.len());
            bincode::ErrorKind::Custom(err)
        })?;
        let hash = solana_sdk::hash::hashv(&[signature.as_ref(), &bincode_serialized_data]);
        Ok(Self {
            signature,
            data,
            hash,
            bincode_serialized_data,
        })
    }

    // Implements bincode::deserialize_from for Vec<CrdsValue>.
    // Verifies that there are exactly as many entries as the encoded size.
    // Rejects trailing bytes.
    pub(crate) fn bincode_deserialize_many(
        bytes: &[u8],
    ) -> impl Iterator<Item = Result<Self, bincode::Error>> + '_ {
        // Decode number of items in the slice.
        // bincode::serialize{,_into} serializes sequence length as
        // a u64 (8 bytes) with fixint encoding in little endian.
        let (size, mut bytes) = match convert_fixed_bytes::<[u8; 8], 8>(bytes) {
            Ok(out) => out,
            Err(err) => {
                return Either::Left(std::iter::once(Err(bincode::Error::from(err))));
            }
        };
        let size = u64::from_le_bytes(size);
        // If size is zero reject trailing bytes.
        if size == 0 && !bytes.is_empty() {
            return Either::Left(std::iter::once(Err(bincode::Error::from(
                bincode::ErrorKind::Custom(String::from("Zero length sequence has trailing bytes")),
            ))));
        }
        // Decode exactly size many items.
        let mut count = 0;
        Either::Right(
            std::iter::repeat_with(move || {
                count += 1;
                let allow_trailing_bytes = count < size;
                Self::bincode_deserialize(bytes, allow_trailing_bytes).inspect(|value| {
                    let offset = value.bincode_serialized_size();
                    bytes = bytes.get(offset..).unwrap();
                })
            })
            .take(size as usize),
        )
    }

    pub fn label(&self) -> CrdsValueLabel {
        let pubkey = self.data.pubkey();
        match self.data {
            CrdsData::LegacyContactInfo(_) => CrdsValueLabel::LegacyContactInfo(pubkey),
            CrdsData::Vote(ix, _) => CrdsValueLabel::Vote(ix, pubkey),
            CrdsData::LowestSlot(_, _) => CrdsValueLabel::LowestSlot(pubkey),
            CrdsData::LegacySnapshotHashes(_) => CrdsValueLabel::LegacySnapshotHashes(pubkey),
            CrdsData::AccountsHashes(_) => CrdsValueLabel::AccountsHashes(pubkey),
            CrdsData::EpochSlots(ix, _) => CrdsValueLabel::EpochSlots(ix, pubkey),
            CrdsData::LegacyVersion(_) => CrdsValueLabel::LegacyVersion(pubkey),
            CrdsData::Version(_) => CrdsValueLabel::Version(pubkey),
            CrdsData::NodeInstance(_) => CrdsValueLabel::NodeInstance(pubkey),
            CrdsData::DuplicateShred(ix, _) => CrdsValueLabel::DuplicateShred(ix, pubkey),
            CrdsData::SnapshotHashes(_) => CrdsValueLabel::SnapshotHashes(pubkey),
            CrdsData::ContactInfo(_) => CrdsValueLabel::ContactInfo(pubkey),
            CrdsData::RestartLastVotedForkSlots(_) => {
                CrdsValueLabel::RestartLastVotedForkSlots(pubkey)
            }
            CrdsData::RestartHeaviestFork(_) => CrdsValueLabel::RestartHeaviestFork(pubkey),
        }
    }

    pub(crate) fn contact_info(&self) -> Option<&ContactInfo> {
        let CrdsData::ContactInfo(node) = &self.data else {
            return None;
        };
        Some(node)
    }

    pub(crate) fn epoch_slots(&self) -> Option<&EpochSlots> {
        let CrdsData::EpochSlots(_, epoch_slots) = &self.data else {
            return None;
        };
        Some(epoch_slots)
    }

    /// Returns the bincode serialized size (in bytes) of the CrdsValue.
    pub fn bincode_serialized_size(&self) -> usize {
        SIGNATURE_BYTES + self.bincode_serialized_data.len()
    }

    /// Returns true if, regardless of prunes, this crds-value
    /// should be pushed to the receiving node.
    pub(crate) fn should_force_push(&self, peer: &Pubkey) -> bool {
        matches!(self.data, CrdsData::NodeInstance(_)) && &self.pubkey() == peer
    }
}

impl Encode for CrdsValue {
    fn encode<W: Write>(&self, mut writer: W) -> Result<(), bincode::Error> {
        Ok(self.bincode_serialize(&mut writer)?)
    }
}

// Converts first N bytes into a value of type T: From<[u8; N]>,
// returning along with the remaining bytes.
pub(crate) fn convert_fixed_bytes<T, const N: usize>(bytes: &[u8]) -> Result<(T, &[u8]), IoError>
where
    T: From<[u8; N]>,
{
    let (bytes, rest) = (N <= bytes.len())
        .then(|| bytes.split_at(N))
        .ok_or_else(|| IoError::from(IoErrorKind::UnexpectedEof))?;
    let value = <[u8; N]>::try_from(bytes).map(T::from).unwrap();
    Ok((value, rest))
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::crds_data::{LowestSlot, NodeInstance, Vote},
        rand0_7::{Rng, SeedableRng},
        rand_chacha0_2::ChaChaRng,
        solana_perf::test_tx::new_test_vote_tx,
        solana_sdk::{
            signature::{Keypair, Signer},
            timing::timestamp,
            vote::state::TowerSync,
        },
        solana_vote_program::{vote_state::Lockout, vote_transaction::new_tower_sync_transaction},
        std::str::FromStr,
    };

    #[test]
    fn test_keys_and_values() {
        let mut rng = rand::thread_rng();
        let v = CrdsValue::new_unsigned(CrdsData::ContactInfo(ContactInfo::default()));
        assert_eq!(v.wallclock(), 0);
        let key = *v.contact_info().unwrap().pubkey();
        assert_eq!(v.label(), CrdsValueLabel::ContactInfo(key));

        let v = Vote::new(Pubkey::default(), new_test_vote_tx(&mut rng), 0).unwrap();
        let v = CrdsValue::new_unsigned(CrdsData::Vote(0, v));
        assert_eq!(v.wallclock(), 0);
        let key = match &v.data {
            CrdsData::Vote(_, vote) => vote.from,
            _ => panic!(),
        };
        assert_eq!(v.label(), CrdsValueLabel::Vote(0, key));

        let v = CrdsValue::new_unsigned(CrdsData::LowestSlot(
            0,
            LowestSlot::new(Pubkey::default(), 0, 0),
        ));
        assert_eq!(v.wallclock(), 0);
        let key = match &v.data {
            CrdsData::LowestSlot(_, data) => data.from,
            _ => panic!(),
        };
        assert_eq!(v.label(), CrdsValueLabel::LowestSlot(key));
    }

    #[test]
    fn test_signature() {
        let mut rng = rand::thread_rng();
        let keypair = Keypair::new();
        let wrong_keypair = Keypair::new();
        let mut v = CrdsValue::new_unsigned(CrdsData::ContactInfo(ContactInfo::new_localhost(
            &keypair.pubkey(),
            timestamp(),
        )));
        verify_signatures(&mut v, &keypair, &wrong_keypair);
        let v = Vote::new(keypair.pubkey(), new_test_vote_tx(&mut rng), timestamp()).unwrap();
        let mut v = CrdsValue::new_unsigned(CrdsData::Vote(0, v));
        verify_signatures(&mut v, &keypair, &wrong_keypair);
        v = CrdsValue::new_unsigned(CrdsData::LowestSlot(
            0,
            LowestSlot::new(keypair.pubkey(), 0, timestamp()),
        ));
        verify_signatures(&mut v, &keypair, &wrong_keypair);
    }

    fn serialize_deserialize_value(value: &mut CrdsValue, keypair: &Keypair) {
        let num_tries = 10;
        value.sign(keypair);
        let original_signature = value.get_signature();
        for _ in 0..num_tries {
            let serialized_value = {
                let mut buffer = Vec::<u8>::new();
                value.bincode_serialize(&mut buffer).unwrap();
                buffer
            };
            let deserialized_value = CrdsValue::bincode_deserialize(
                &serialized_value,
                false, // allow_trailing_bytes
            )
            .unwrap();

            // Signatures shouldn't change
            let deserialized_signature = deserialized_value.get_signature();
            assert_eq!(original_signature, deserialized_signature);

            // After deserializing, check that the signature is still the same
            assert!(deserialized_value.verify());
        }
    }

    fn verify_signatures(
        value: &mut CrdsValue,
        correct_keypair: &Keypair,
        wrong_keypair: &Keypair,
    ) {
        assert!(!value.verify());
        value.sign(correct_keypair);
        assert!(value.verify());
        value.sign(wrong_keypair);
        assert!(!value.verify());
        serialize_deserialize_value(value, correct_keypair);
    }

    #[test]
    fn test_should_force_push() {
        let mut rng = rand::thread_rng();
        let pubkey = Pubkey::new_unique();
        assert!(
            !CrdsValue::new_unsigned(CrdsData::ContactInfo(ContactInfo::new_rand(
                &mut rng,
                Some(pubkey)
            )))
            .should_force_push(&pubkey)
        );
        let node = CrdsValue::new_unsigned(CrdsData::NodeInstance(NodeInstance::new(
            &mut rng,
            pubkey,
            timestamp(),
        )));
        assert!(node.should_force_push(&pubkey));
        assert!(!node.should_force_push(&Pubkey::new_unique()));
    }

    #[test]
    fn test_serialize_round_trip() {
        let mut rng = ChaChaRng::from_seed(
            bs58::decode("4nHgVgCvVaHnsrg4dYggtvWYYgV3JbeyiRBWupPMt3EG")
                .into_vec()
                .map(<[u8; 32]>::try_from)
                .unwrap()
                .unwrap(),
        );
        let values: Vec<CrdsValue> = vec![
            {
                let keypair = Keypair::generate(&mut rng);
                let lockouts: [Lockout; 4] = [
                    Lockout::new_with_confirmation_count(302_388_991, 11),
                    Lockout::new_with_confirmation_count(302_388_995, 7),
                    Lockout::new_with_confirmation_count(302_389_001, 3),
                    Lockout::new_with_confirmation_count(302_389_005, 1),
                ];
                let tower_sync = TowerSync {
                    lockouts: lockouts.into_iter().collect(),
                    root: Some(302_388_989),
                    hash: Hash::new_from_array(rng.gen()),
                    timestamp: Some(1_732_044_716_167),
                    block_id: Hash::new_from_array(rng.gen()),
                };
                let vote = new_tower_sync_transaction(
                    tower_sync,
                    Hash::new_from_array(rng.gen()), // blockhash
                    &keypair,                        // node_keypair
                    &Keypair::generate(&mut rng),    // vote_keypair
                    &Keypair::generate(&mut rng),    // authorized_voter_keypair
                    None,                            // switch_proof_hash
                );
                let vote = Vote::new(
                    keypair.pubkey(),
                    vote,
                    1_732_045_236_371, // wallclock
                )
                .unwrap();
                CrdsValue::new(CrdsData::Vote(5, vote), &keypair)
            },
            {
                let keypair = Keypair::generate(&mut rng);
                let lockouts: [Lockout; 3] = [
                    Lockout::new_with_confirmation_count(302_410_500, 9),
                    Lockout::new_with_confirmation_count(302_410_505, 5),
                    Lockout::new_with_confirmation_count(302_410_517, 1),
                ];
                let tower_sync = TowerSync {
                    lockouts: lockouts.into_iter().collect(),
                    root: Some(302_410_499),
                    hash: Hash::new_from_array(rng.gen()),
                    timestamp: Some(1_732_053_615_237),
                    block_id: Hash::new_from_array(rng.gen()),
                };
                let vote = new_tower_sync_transaction(
                    tower_sync,
                    Hash::new_from_array(rng.gen()), // blockhash
                    &keypair,                        // node_keypair
                    &Keypair::generate(&mut rng),    // vote_keypair
                    &Keypair::generate(&mut rng),    // authorized_voter_keypair
                    None,                            // switch_proof_hash
                );
                let vote = Vote::new(
                    keypair.pubkey(),
                    vote,
                    1_732_053_639_350, // wallclock
                )
                .unwrap();
                CrdsValue::new(CrdsData::Vote(5, vote), &keypair)
            },
        ];
        let mut bytes = Vec::<u8>::new();
        CrdsValue::bincode_serialize_many(&values, &mut bytes).unwrap();
        // Serialized bytes are fixed and should never change.
        assert_eq!(
            solana_sdk::hash::hash(&bytes),
            Hash::from_str("7gtcoafccWE964njbs2bA1QuVFeV34RaoY781yLx2A8N").unwrap()
        );
        // serialize -> deserialize should round trip.
        assert_eq!(
            CrdsValue::bincode_deserialize_many(&bytes)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            values
        );
        // More entries than the encoded size should fail deserialization.
        for size in 0..2u64 {
            bytes[..8].copy_from_slice(&size.to_le_bytes());
            assert_matches!(
                CrdsValue::bincode_deserialize_many(&bytes).collect::<Result<Vec<_>, _>>(),
                Err(err) if matches!(*err, bincode::ErrorKind::Custom(_))
            );
        }
        // Fewer entries than the encoded size should fail deserialization.
        for size in 3..5u64 {
            bytes[..8].copy_from_slice(&size.to_le_bytes());
            assert_matches!(
                CrdsValue::bincode_deserialize_many(&bytes)
                    .collect::<Result<Vec<_>, _>>(),
                Err(err) if matches!(*err, bincode::ErrorKind::Io(_))
            );
        }
        // Back to the right size.
        bytes[..8].copy_from_slice(&2u64.to_le_bytes());
        assert_eq!(
            CrdsValue::bincode_deserialize_many(&bytes)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            values
        );
    }
}
