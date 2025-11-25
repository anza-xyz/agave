use {
    crate::{
        contact_info::ContactInfo,
        crds_data::{CrdsData, EpochSlotsIndex, VoteIndex},
        duplicate_shred::DuplicateShredIndex,
        epoch_slots::EpochSlots,
    },
    arrayvec::ArrayVec,
    bincode::serialize,
    rand::Rng,
    serde::{de::Deserializer, Deserialize, Serialize},
    solana_hash::Hash,
    solana_keypair::{signable::Signable, Keypair},
    solana_packet::PACKET_DATA_SIZE,
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    solana_signature::Signature,
    solana_signer::Signer,
    std::borrow::{Borrow, Cow},
};

/// CrdsValue that is replicated across the cluster
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct CrdsValue {
    signature: Signature,
    data: CrdsData,
    #[serde(skip_serializing)]
    hash: Hash, // Sha256 hash of [signature, data].
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

    fn signable_data(&self) -> Cow<'static, [u8]> {
        Cow::Owned(serialize(&self.data).expect("failed to serialize CrdsData"))
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
        let hash = solana_sha256_hasher::hashv(&[signature.as_ref(), &bincode_serialized_data]);
        Self {
            signature,
            data,
            hash,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_unsigned(data: CrdsData) -> Self {
        let bincode_serialized_data = bincode::serialize(&data).unwrap();
        let signature = Signature::default();
        let hash = solana_sha256_hasher::hashv(&[signature.as_ref(), &bincode_serialized_data]);
        Self {
            signature,
            data,
            hash,
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
        bincode::serialized_size(&self)
            .map(usize::try_from)
            .unwrap()
            .unwrap()
    }
}

// Manual implementation of Deserialize for CrdsValue in order to populate
// CrdsValue.hash which is skipped in serialization.
impl<'de> Deserialize<'de> for CrdsValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct CrdsValue {
            signature: Signature,
            data: CrdsData,
        }
        let CrdsValue { signature, data } = CrdsValue::deserialize(deserializer)?;
        // To compute the hash of the received CrdsData we need to re-serialize it
        // PACKET_DATA_SIZE is always enough since we have just received the value in a packet
        // ArrayVec allows us to write serialized data into stack memory without initializing it
        let mut buffer = ArrayVec::<u8, PACKET_DATA_SIZE>::new();
        bincode::serialize_into(&mut buffer, &data).map_err(serde::de::Error::custom)?;
        let hash = solana_sha256_hasher::hashv(&[signature.as_ref(), &buffer]);
        Ok(Self {
            signature,
            data,
            hash,
        })
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::crds_data::{LowestSlot, Vote},
        bincode::deserialize,
        rand::SeedableRng as _,
        rand_chacha::ChaChaRng,
        solana_keypair::Keypair,
        solana_perf::test_tx::new_test_vote_tx,
        solana_signer::Signer,
        solana_time_utils::timestamp,
        solana_vote::vote_transaction::new_tower_sync_transaction,
        solana_vote_interface::state::TowerSync,
        solana_vote_program::vote_state::Lockout,
        std::str::FromStr,
    };

    #[test]
    fn test_keys_and_values() {
        let mut rng = rand::thread_rng();
        let v = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::default()));
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
        let mut v = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(
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
            let serialized_value = serialize(value).unwrap();
            let deserialized_value: CrdsValue = deserialize(&serialized_value).unwrap();

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
                let keypair = Keypair::new_from_array(rng.gen());
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
                    Hash::new_from_array(rng.gen()),     // blockhash
                    &keypair,                            // node_keypair
                    &Keypair::new_from_array(rng.gen()), // vote_keypair
                    &Keypair::new_from_array(rng.gen()), // authorized_voter_keypair
                    None,                                // switch_proof_hash
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
                let keypair = Keypair::new_from_array(rng.gen());
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
                    Hash::new_from_array(rng.gen()),     // blockhash
                    &keypair,                            // node_keypair
                    &Keypair::new_from_array(rng.gen()), // vote_keypair
                    &Keypair::new_from_array(rng.gen()), // authorized_voter_keypair
                    None,                                // switch_proof_hash
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
        let bytes = bincode::serialize(&values).unwrap();
        // Serialized bytes are fixed and should never change.
        assert_eq!(
            solana_sha256_hasher::hash(&bytes),
            Hash::from_str("BTg284TRo5S5PpbA9YZaab5rKeoLNAj7arwadvG6XVLT").unwrap()
        );
        // serialize -> deserialize should round trip.
        assert_eq!(
            bincode::deserialize::<Vec<CrdsValue>>(&bytes).unwrap(),
            values
        );
    }
}
