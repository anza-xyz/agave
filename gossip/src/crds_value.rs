#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        contact_info::ContactInfo,
        crds_data::{CrdsData, EpochSlotsIndex, VoteIndex},
        duplicate_shred::DuplicateShredIndex,
        epoch_slots::EpochSlots,
        verifying_key_cache::VerifyingKeyCache,
    },
    rand::Rng,
    serde::{Deserialize, Serialize, de::Deserializer},
    solana_hash::Hash,
    solana_keypair::{Keypair, signable::Signable},
    solana_packet::PACKET_DATA_SIZE,
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    solana_signature::Signature,
    solana_signer::Signer,
    std::{
        borrow::{Borrow, Cow},
        mem::MaybeUninit,
    },
    wincode::{ReadError, ReadResult, SchemaRead, SchemaWrite, config::Config, io::Reader},
};

/// CrdsValue that is replicated across the cluster
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "4ABukH5bS69APB3bu1hbMiGyeKPw21nzXAVzCRMtKPih",
        abi_serializer = ["bincode", "wincode"],
        // `hash` is recomputed from [signature, data] on deserialize, so it can't
        // match an independently-sampled value; verify the wire round-trip only.
        test_roundtrip = "wire_only",
    )
)]
#[derive(Serialize, Clone, Debug, PartialEq, Eq, SchemaWrite)]
pub struct CrdsValue {
    signature: Signature,
    data: CrdsData,
    #[serde(skip_serializing)]
    #[wincode(skip)]
    // Not on the wire (recomputed on deserialize); keep it out of the sample.
    #[cfg_attr(feature = "frozen-abi", stable_abi_sample(with = "Hash::default()"))]
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
        Cow::Owned(wincode::serialize(&self.data).expect("failed to serialize CrdsData"))
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
    Vote(VoteIndex, Pubkey),
    LowestSlot(Pubkey),
    EpochSlots(EpochSlotsIndex, Pubkey),
    DuplicateShred(DuplicateShredIndex, Pubkey),
    SnapshotHashes(Pubkey),
    ContactInfo(Pubkey),
    RestartLastVotedForkSlots(Pubkey),
    RestartHeaviestFork(Pubkey),
}

impl CrdsValueLabel {
    pub fn pubkey(&self) -> Pubkey {
        match self {
            CrdsValueLabel::Vote(_, p) => *p,
            CrdsValueLabel::LowestSlot(p) => *p,
            CrdsValueLabel::EpochSlots(_, p) => *p,
            CrdsValueLabel::DuplicateShred(_, p) => *p,
            CrdsValueLabel::SnapshotHashes(p) => *p,
            CrdsValueLabel::ContactInfo(pubkey) => *pubkey,
            CrdsValueLabel::RestartLastVotedForkSlots(p) => *p,
            CrdsValueLabel::RestartHeaviestFork(p) => *p,
        }
    }
}

impl CrdsValue {
    /// Verify the signature, reusing a cached decompressed verifying key.
    /// Inserts only after `verify_strict` succeeds, so the cache can't be
    /// seeded with arbitrary pubkeys to evict useful entries.
    pub(crate) fn verify_with_cache(&self, cache: &VerifyingKeyCache) -> bool {
        let pubkey = self.pubkey();
        let signable_data = self.signable_data();
        let message = signable_data.borrow();
        let sig_bytes: [u8; 64] = self.signature.into();
        let signature = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        match cache.get(&pubkey) {
            Some(vk) => vk.verify_strict(message, &signature).is_ok(),
            None => {
                let Ok(vk) = ed25519_dalek::VerifyingKey::try_from(pubkey.as_ref()) else {
                    return false;
                };
                if vk.verify_strict(message, &signature).is_err() {
                    return false;
                }
                cache.insert(pubkey, vk);
                true
            }
        }
    }

    pub fn new(data: CrdsData, keypair: &Keypair) -> Self {
        let serialized_data = wincode::serialize(&data).unwrap();
        let signature = keypair.sign_message(&serialized_data);
        let hash = solana_sha256_hasher::hashv(&[signature.as_ref(), &serialized_data]);
        Self {
            signature,
            data,
            hash,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_unsigned(data: CrdsData) -> Self {
        let serialized_data = wincode::serialize(&data).unwrap();
        let signature = Signature::default();
        let hash = solana_sha256_hasher::hashv(&[signature.as_ref(), &serialized_data]);
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
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
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
            CrdsData::Vote(ix, _) => CrdsValueLabel::Vote(ix, pubkey),
            CrdsData::LowestSlot(_, _) => CrdsValueLabel::LowestSlot(pubkey),
            CrdsData::EpochSlots(ix, _) => CrdsValueLabel::EpochSlots(ix, pubkey),
            CrdsData::DuplicateShred(ix, _) => CrdsValueLabel::DuplicateShred(ix, pubkey),
            CrdsData::SnapshotHashes(_) => CrdsValueLabel::SnapshotHashes(pubkey),
            CrdsData::ContactInfo(_) => CrdsValueLabel::ContactInfo(pubkey),
            CrdsData::RestartLastVotedForkSlots(_) => {
                CrdsValueLabel::RestartLastVotedForkSlots(pubkey)
            }
            CrdsData::RestartHeaviestFork(_) => CrdsValueLabel::RestartHeaviestFork(pubkey),
            // Deprecated: sanitize() rejects these before any caller reaches here.
            CrdsData::LegacyContactInfo(_)
            | CrdsData::LegacySnapshotHashes(_)
            | CrdsData::AccountsHashes(_)
            | CrdsData::LegacyVersion(_)
            | CrdsData::Version(_)
            | CrdsData::NodeInstance(_) => unreachable!("deprecated CrdsData variant"),
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

    /// Returns the serialized size (in bytes) of the CrdsValue.
    pub fn serialized_size(&self) -> usize {
        wincode::serialized_size(&self)
            .map(usize::try_from)
            .unwrap()
            .unwrap()
    }
}

// Computes sha256(signature || serialize(data)) using a stack buffer.
// PACKET_DATA_SIZE is always enough since the value originated in a packet.
fn compute_crds_value_hash(signature: &Signature, data: &CrdsData) -> wincode::WriteResult<Hash> {
    let mut buffer = [MaybeUninit::<u8>::uninit(); PACKET_DATA_SIZE];
    let mut writer: &mut [MaybeUninit<u8>] = &mut buffer;
    wincode::serialize_into(&mut writer, data)?;
    let written = PACKET_DATA_SIZE - writer.len();
    // SAFETY: wincode's "Writer for &mut [MaybeUninit<u8>]" initializes every
    // consumed slot before advancing the cursor, so the first "written" bytes are init.
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), written) };
    Ok(solana_sha256_hasher::hashv(&[signature.as_ref(), bytes]))
}

// Manual implementation of SchemaRead for CrdsValue in order to populate
// CrdsValue.hash which is skipped in serialization.
unsafe impl<'de, C: Config> SchemaRead<'de, C> for CrdsValue {
    type Dst = Self;
    fn read(reader: impl Reader<'de>, dst: &mut MaybeUninit<Self>) -> ReadResult<()> {
        #[derive(SchemaRead)]
        struct CrdsValueLite {
            signature: Signature,
            data: CrdsData,
        }
        let CrdsValueLite { signature, data } = <CrdsValueLite as SchemaRead<'de, C>>::get(reader)?;
        let hash = compute_crds_value_hash(&signature, &data)
            .map_err(|_| ReadError::Custom("failed to serialize CrdsData"))?;
        dst.write(Self {
            signature,
            data,
            hash,
        });
        Ok(())
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
        let hash = compute_crds_value_hash(&signature, &data).map_err(serde::de::Error::custom)?;
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
    fn test_verify_with_cache() {
        let keypair = Keypair::new();
        let wrong_keypair = Keypair::new();
        let mut value = CrdsValue::new_unsigned(CrdsData::from(ContactInfo::new_localhost(
            &keypair.pubkey(),
            0,
        )));
        let cache = VerifyingKeyCache::new();

        assert!(
            !value.verify_with_cache(&cache),
            "unsigned value must not verify"
        );
        assert!(
            cache.get(&value.pubkey()).is_none(),
            "failed verification must not populate the cache"
        );

        // Wrong signature: rejected, and nothing cached. Checked before any valid
        // insert so we know the rejection itself didn't seed the cache.
        value.sign(&wrong_keypair);
        assert!(
            !value.verify_with_cache(&cache),
            "value signed by the wrong key must not verify"
        );
        assert!(
            cache.get(&value.pubkey()).is_none(),
            "failed verification must not populate the cache"
        );

        // Cold miss: a valid signature verifies and populates the cache.
        value.sign(&keypair);
        assert!(
            value.verify_with_cache(&cache),
            "validly signed value must verify"
        );
        assert!(
            cache.get(&value.pubkey()).is_some(),
            "successful verification must populate the cache"
        );

        // Warm hit: verifies again, now served from the cached key.
        assert!(
            value.verify_with_cache(&cache),
            "validly signed value must verify on a cache hit"
        );
    }

    #[test]
    fn test_keys_and_values() {
        let mut rng = rand::rng();
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
        let mut rng = rand::rng();
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

    fn serialize_deserialize_value(
        value: &mut CrdsValue,
        keypair: &Keypair,
        cache: &VerifyingKeyCache,
    ) {
        let num_tries = 10;
        value.sign(keypair);
        let original_signature = value.get_signature();
        for _ in 0..num_tries {
            let serialized_value = wincode::serialize(value).unwrap();
            assert_eq!(serialized_value, bincode::serialize(value).unwrap());
            let deserialized_value: CrdsValue = wincode::deserialize(&serialized_value).unwrap();
            assert_eq!(
                deserialized_value,
                bincode::deserialize::<CrdsValue>(&serialized_value).unwrap()
            );

            // Signatures shouldn't change
            let deserialized_signature = deserialized_value.get_signature();
            assert_eq!(original_signature, deserialized_signature);

            // After deserializing, check that the signature is still the same
            assert!(deserialized_value.verify_with_cache(cache));
        }
    }

    fn verify_signatures(
        value: &mut CrdsValue,
        correct_keypair: &Keypair,
        wrong_keypair: &Keypair,
    ) {
        let cache = VerifyingKeyCache::new();
        assert!(!value.verify_with_cache(&cache));
        value.sign(correct_keypair);
        assert!(value.verify_with_cache(&cache));
        value.sign(wrong_keypair);
        assert!(!value.verify_with_cache(&cache));
        serialize_deserialize_value(value, correct_keypair, &cache);
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
                let keypair = Keypair::new_from_array(rng.random());
                let lockouts: [Lockout; 4] = [
                    Lockout::new_with_confirmation_count(302_388_991, 11),
                    Lockout::new_with_confirmation_count(302_388_995, 7),
                    Lockout::new_with_confirmation_count(302_389_001, 3),
                    Lockout::new_with_confirmation_count(302_389_005, 1),
                ];
                let tower_sync = TowerSync {
                    lockouts: lockouts.into_iter().collect(),
                    root: Some(302_388_989),
                    hash: Hash::new_from_array(rng.random()),
                    timestamp: Some(1_732_044_716_167),
                    block_id: Hash::new_from_array(rng.random()),
                };
                let vote = new_tower_sync_transaction(
                    tower_sync,
                    Hash::new_from_array(rng.random()), // blockhash
                    &keypair,                           // node_keypair
                    &Keypair::new_from_array(rng.random()), // vote_keypair
                    &Keypair::new_from_array(rng.random()), // authorized_voter_keypair
                    None,                               // switch_proof_hash
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
                let keypair = Keypair::new_from_array(rng.random());
                let lockouts: [Lockout; 3] = [
                    Lockout::new_with_confirmation_count(302_410_500, 9),
                    Lockout::new_with_confirmation_count(302_410_505, 5),
                    Lockout::new_with_confirmation_count(302_410_517, 1),
                ];
                let tower_sync = TowerSync {
                    lockouts: lockouts.into_iter().collect(),
                    root: Some(302_410_499),
                    hash: Hash::new_from_array(rng.random()),
                    timestamp: Some(1_732_053_615_237),
                    block_id: Hash::new_from_array(rng.random()),
                };
                let vote = new_tower_sync_transaction(
                    tower_sync,
                    Hash::new_from_array(rng.random()), // blockhash
                    &keypair,                           // node_keypair
                    &Keypair::new_from_array(rng.random()), // vote_keypair
                    &Keypair::new_from_array(rng.random()), // authorized_voter_keypair
                    None,                               // switch_proof_hash
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
        let bytes = wincode::serialize(&values).unwrap();
        assert_eq!(bytes, bincode::serialize(&values).unwrap());
        // Serialized bytes are fixed and should never change.
        assert_eq!(
            solana_sha256_hasher::hash(&bytes),
            Hash::from_str("BTg284TRo5S5PpbA9YZaab5rKeoLNAj7arwadvG6XVLT").unwrap()
        );
        // serialize -> deserialize should round trip.
        let wincode_values = wincode::deserialize::<Vec<CrdsValue>>(&bytes).unwrap();
        assert_eq!(
            wincode_values,
            bincode::deserialize::<Vec<CrdsValue>>(&bytes).unwrap()
        );
        assert_eq!(wincode_values, values);
    }

    #[test]
    fn test_wincode_compatibility_crds_value() {
        let mut rng = rand::rng();
        for _ in 0..1000 {
            let value = CrdsValue::new_rand(&mut rng, None);
            let bincode_bytes = bincode::serialize(&value).unwrap();
            let wincode_bytes = wincode::serialize(&value).unwrap();
            assert_eq!(
                bincode_bytes,
                wincode_bytes,
                "bytes differ for {:?}",
                value.label()
            );
            // Deprecated types and Vote with test-only invalid transactions intentionally
            // fail serde deserialization; skip those.
            let Ok(bincode_decoded) = bincode::deserialize::<CrdsValue>(&bincode_bytes) else {
                continue;
            };
            let wincode_decoded: CrdsValue = wincode::deserialize(&bincode_bytes)
                .unwrap_or_else(|e| panic!("wincode deser failed for {:?}: {e}", value.label()));
            assert_eq!(
                bincode_decoded,
                wincode_decoded,
                "deser mismatch for {:?}",
                value.label()
            );
            assert_eq!(value, bincode_decoded);
        }
    }
}
