pub(super) use partial_cert::CertificateStats;
use {
    super::BuildRewardCertsRespError,
    crate::block_creation_loop::rewards::{
        certs_builder::entry::partial_cert::BuildResult, msg_types::RewardRespSucc,
    },
    agave_bls_sigverify::sig_verified_messages::VoteAggregate,
    agave_votor::aggregate_accumulator::AggregateAccumulatorError,
    agave_votor_messages::{
        consensus_message::VoteMessage, reward_certificate::SkipRewardCertificate, vote::Vote,
    },
    bitvec::vec::BitVec,
    notar_entry::NotarEntry,
    partial_cert::PartialCert,
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    std::sync::Arc,
};

mod notar_entry;
mod partial_cert;

#[derive(Clone)]
/// Per slot container for storing notar and skip votes for creating rewards certificates.
pub(super) struct Entry {
    /// [`PartialCert`] for observed skip votes.
    skip: PartialCert,
    /// Struct to store state for observed notar votes.
    notar: NotarEntry,
    /// Rank map of the slot this entry is working on.
    rank_map: Arc<BLSPubkeyToRankMap>,
}

impl Entry {
    /// Creates a new instance of [`Entry`].
    pub(super) fn new(rank_map: Arc<BLSPubkeyToRankMap>) -> Self {
        Self {
            skip: PartialCert::new(rank_map.len()),
            notar: NotarEntry::new(),
            rank_map,
        }
    }

    /// Adds the given [`VoteAggregate`] from another validator to the aggregate.
    pub(super) fn add_aggregate(
        &mut self,
        aggregate: VoteAggregate,
        vote_account_pubkeys: Vec<Pubkey>,
    ) -> Result<(), AggregateAccumulatorError> {
        match *aggregate.vote() {
            Vote::Notarize(notar) => self.notar.add_aggregate(
                aggregate,
                vote_account_pubkeys,
                notar.block.block_id,
                self.rank_map.len(),
            ),
            Vote::Skip(_) => self.skip.add_aggregate(aggregate, vote_account_pubkeys),
            _ => Ok(()),
        }
    }

    /// Adds the given [`VoteMessage`] from this node itself to the aggregate.
    pub(super) fn add_own_msg(
        &mut self,
        vote_msg: VoteMessage,
        vote_account_pubkey: Pubkey,
    ) -> Result<(), AggregateAccumulatorError> {
        match vote_msg.vote {
            Vote::Notarize(notar) => self.notar.add_own_msg(
                vote_msg,
                vote_account_pubkey,
                notar.block.block_id,
                self.rank_map.len(),
            ),
            Vote::Skip(_) => self.skip.add_own_msg(vote_msg, vote_account_pubkey),
            _ => Ok(()),
        }
    }

    /// Adds a verified certificate from another validator, whose `ranks` all signed `vote`.
    pub(super) fn add_certificate(
        &mut self,
        vote: Vote,
        ranks: &BitVec<u8>,
        signature: BLSSignature,
    ) {
        match vote {
            Vote::Notarize(notar) => self.notar.add_certificate(
                ranks,
                signature,
                notar.block.block_id,
                self.rank_map.len(),
            ),
            Vote::Skip(_) => self.skip.add_certificate(ranks, signature),
            _ => (),
        }
    }

    /// Builds reward certificates from the observed votes, and from certificates of other
    /// validators that contain votes this node missed.
    pub(super) fn build_certs(
        self,
        reward_slot: Slot,
        stats: &mut CertificateStats,
    ) -> Result<RewardRespSucc, BuildRewardCertsRespError> {
        let notar = self.notar.build_cert(reward_slot, &self.rank_map, stats)?;
        let skip = match self.skip.build(&self.rank_map, stats) {
            BuildResult::Empty | BuildResult::Identity => None,
            BuildResult::EncodingError(e) => return Err(BuildRewardCertsRespError::Encoding(e)),
            BuildResult::Success {
                signature,
                bitmap,
                validators,
            } => {
                let cert = SkipRewardCertificate::try_new(reward_slot, signature, bitmap)?;
                Some((cert, validators))
            }
        };

        let (skip, notar, validators) = match (skip, notar) {
            (None, None) => (None, None, vec![]),
            (Some((skip_cert, skip_validators)), None) => (Some(skip_cert), None, skip_validators),
            (None, Some((notar_cert, notar_validators))) => {
                (None, Some(notar_cert), notar_validators)
            }
            (Some((skip_cert, skip_validators)), Some((notar_cert, notar_validators))) => {
                let mut validators = skip_validators;
                validators.extend(notar_validators);
                (Some(skip_cert), Some(notar_cert), validators)
            }
        };

        Ok(RewardRespSucc {
            skip,
            notar,
            validators,
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::block_creation_loop::rewards::certs_builder::CertsBuilder,
        agave_bls_sigverify::rewards::RewardInput,
        agave_votor_messages::{
            certificate::{Certificate, CertificateType},
            consensus_message::{Block, BlockId},
            reward_certificate::NUM_SLOTS_FOR_REWARD,
            stake::NonZeroStake,
            vote::Vote,
            wire::{VotePayloadToSign, get_vote_payload_to_sign},
        },
        rand::Rng,
        solana_bls_signatures::{
            Keypair as BlsKeypair, PubkeyCompressed as BlsPubkeyCompressed, SignatureProjective,
        },
        solana_epoch_schedule::EpochSchedule,
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::{Bank, SlotLeader},
            genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
            validated_reward_certificate::ValidatedRewardCert,
        },
        solana_signer::Signer,
        solana_signer_store::{Decoded, decode, encode_base2},
        std::collections::HashMap,
    };

    pub(crate) fn validate_bitmap(bitmap: &[u8], num_set: usize, max_len: usize) {
        let bitvec = decode(bitmap, max_len).unwrap();
        match bitvec {
            Decoded::Base2(bitvec) => assert_eq!(bitvec.count_ones(), num_set),
            Decoded::Base3(_, _) => panic!("unexpected variant"),
        }
    }

    pub(crate) fn new_reward_vote_aggregate(
        vote: Vote,
        rank: usize,
        keypairs: &[BlsKeypair],
        stakes: Option<&[u64]>,
        shred_version: u16,
    ) -> (VoteAggregate, Vec<Pubkey>) {
        let serialized = get_vote_payload_to_sign(vote, shred_version);
        let signature = keypairs[rank].sign(&serialized).into();
        let stake = match stakes {
            None => NonZeroStake::new(123).unwrap(),
            Some(stakes) => NonZeroStake::new(stakes[rank]).unwrap(),
        };
        let vote_msg = VoteMessage {
            vote,
            signature,
            rank: rank.try_into().unwrap(),
            stake,
        };
        let aggregate = VoteAggregate::new_from_verified_vote(keypairs.len(), vote_msg);
        (aggregate, vec![Pubkey::new_unique()])
    }

    pub(crate) fn new_identity_reward_vote_aggregate(
        vote: Vote,
        max_validators: usize,
        ranks_and_stakes: &[(u16, u64)],
        shred_version: u16,
    ) -> (VoteAggregate, Vec<Pubkey>) {
        // This models the aggregate produced by individually valid votes whose signatures cancel.
        let aggregate = VoteAggregate::new_from_verified_votes(
            max_validators,
            VotePayloadToSign::new_from_vote(vote, shred_version),
            ranks_and_stakes.iter().map(|(rank, stake)| {
                (
                    *rank,
                    NonZeroStake::new(*stake).expect("test stake must be nonzero"),
                )
            }),
            SignatureProjective::identity(),
        );
        let validators = ranks_and_stakes
            .iter()
            .map(|_| Pubkey::new_unique())
            .collect();
        (aggregate, validators)
    }

    pub(crate) fn get_keypairs(max_validators: usize, slot: Slot) -> Vec<BlsKeypair> {
        get_keypair_with_stakes(vec![100; max_validators], slot)
    }

    pub(crate) fn get_keypair_with_stakes(stakes: Vec<u64>, slot: Slot) -> Vec<BlsKeypair> {
        get_rank_map_and_keypairs(stakes, slot).1
    }

    /// Returns the rank map of `slot`, and the validators' BLS keypairs by rank.
    pub(crate) fn get_rank_map_and_keypairs(
        stakes: Vec<u64>,
        slot: Slot,
    ) -> (Arc<BLSPubkeyToRankMap>, Vec<BlsKeypair>) {
        let (_, rank_map, keypairs) = get_reward_bank(stakes, slot);
        (rank_map, keypairs)
    }

    /// Returns the bank of the slot whose block carries the reward certs of `slot`, the rank map
    /// of `slot`, and the validators' BLS keypairs by rank.
    pub(crate) fn get_reward_bank(
        stakes: Vec<u64>,
        slot: Slot,
    ) -> (Arc<Bank>, Arc<BLSPubkeyToRankMap>, Vec<BlsKeypair>) {
        let max_validators = stakes.len();
        let validator_keypairs = (0..max_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let keypair_map = validator_keypairs
            .iter()
            .map(|k| {
                (
                    BlsPubkeyCompressed::from(k.bls_keypair.public.into_inner()),
                    k.bls_keypair.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut genesis_config =
            create_genesis_config_with_vote_accounts(1_000_000_000, &validator_keypairs, stakes)
                .genesis_config;
        genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::default(),
            slot + NUM_SLOTS_FOR_REWARD,
        );
        let rank_map = bank.get_rank_map(slot).unwrap().clone();
        let keypairs = (0..max_validators)
            .map(|index| {
                let pubkey_affine = rank_map.get_pubkey_stake_entry(index).unwrap().bls_pubkey;
                keypair_map
                    .get(&BlsPubkeyCompressed::from(*pubkey_affine))
                    .unwrap()
                    .clone()
            })
            .collect();
        (bank, rank_map, keypairs)
    }

    fn build(entry: Entry, slot: Slot) -> RewardRespSucc {
        entry
            .build_certs(slot, &mut CertificateStats::default())
            .unwrap()
    }

    #[test]
    fn validate_build_skip_cert() {
        let slot = 123;
        let max_validators = 5;
        let (rank_map, keypairs) = get_rank_map_and_keypairs(vec![100; max_validators], slot);
        let shred_version = rand::rng().random();
        let mut entry = Entry::new(rank_map);
        let resp = build(entry.clone(), slot);
        assert_eq!(resp.skip, None);
        assert_eq!(resp.notar, None);

        let skip = Vote::new_skip_vote(7);
        let (aggregate, vote_account_pubkeys) =
            new_reward_vote_aggregate(skip, 0, &keypairs, None, shred_version);
        entry
            .add_aggregate(aggregate, vote_account_pubkeys)
            .unwrap();
        let resp = build(entry, slot);
        assert_eq!(resp.notar, None);
        let skip = resp.skip.unwrap();
        assert_eq!(skip.slot, slot);
        validate_bitmap(skip.to_bitmap(), 1, 5);
    }

    #[test]
    fn validate_build_notar_cert() {
        let slot = 123;
        let max_validators = 5;
        let shred_version = rand::rng().random();
        let (rank_map, keypairs) = get_rank_map_and_keypairs(vec![100; max_validators], slot);

        let mut entry = Entry::new(rank_map);
        let resp = build(entry.clone(), slot);
        assert_eq!(resp.skip, None);
        assert_eq!(resp.notar, None);

        let blockid0 = BlockId::new_unique();
        let blockid1 = BlockId::new_unique();

        for rank in 0..2 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid0,
            });
            let (aggregate, vote_account_pubkeys) =
                new_reward_vote_aggregate(notar, rank, &keypairs, None, shred_version);
            entry
                .add_aggregate(aggregate, vote_account_pubkeys)
                .unwrap();
        }
        for rank in 2..5 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid1,
            });
            let (aggregate, vote_account_pubkeys) =
                new_reward_vote_aggregate(notar, rank, &keypairs, None, shred_version);
            entry
                .add_aggregate(aggregate, vote_account_pubkeys)
                .unwrap();
        }
        let resp = build(entry, slot);
        assert_eq!(resp.skip, None);
        let notar = resp.notar.unwrap();
        assert_eq!(notar.slot, slot);
        assert_eq!(notar.block_id, blockid1);
        validate_bitmap(notar.bitmap(), 3, 5);
    }

    #[test]
    fn identity_skip_cert_is_omitted_without_dropping_notar_cert() {
        let slot = 123;
        let max_validators = 3;
        let shred_version = rand::rng().random();
        let keypairs = (0..max_validators)
            .map(|_| BlsKeypair::new())
            .collect::<Vec<_>>();
        let (rank_map, _) = get_rank_map_and_keypairs(vec![100; max_validators], slot);
        let mut entry = Entry::new(rank_map);

        let skip = Vote::new_skip_vote(slot);
        let (aggregate, skip_validators) = new_identity_reward_vote_aggregate(
            skip,
            max_validators,
            &[(0, 100), (1, 100)],
            shred_version,
        );
        entry.add_aggregate(aggregate, skip_validators).unwrap();

        let block_id = BlockId::new_unique();
        let notar = Vote::new_notarization_vote(Block { slot, block_id });
        let (aggregate, notar_validators) =
            new_reward_vote_aggregate(notar, 2, &keypairs, None, shred_version);
        entry
            .add_aggregate(aggregate, notar_validators.clone())
            .unwrap();

        let resp = build(entry, slot);
        assert!(resp.skip.is_none());
        assert_eq!(resp.notar.unwrap().block_id, block_id);
        assert_eq!(resp.validators, notar_validators);
    }

    #[test]
    fn identity_notar_cert_is_omitted_without_dropping_skip_cert() {
        let slot = 123;
        let max_validators = 3;
        let shred_version = rand::rng().random();
        let keypairs = (0..max_validators)
            .map(|_| BlsKeypair::new())
            .collect::<Vec<_>>();
        let (rank_map, _) = get_rank_map_and_keypairs(vec![100; max_validators], slot);
        let mut entry = Entry::new(rank_map);

        let block_id = BlockId::new_unique();
        let notar = Vote::new_notarization_vote(Block { slot, block_id });
        let (aggregate, notar_validators) = new_identity_reward_vote_aggregate(
            notar,
            max_validators,
            &[(0, 100), (1, 100)],
            shred_version,
        );
        entry.add_aggregate(aggregate, notar_validators).unwrap();

        let skip = Vote::new_skip_vote(slot);
        let (aggregate, skip_validators) =
            new_reward_vote_aggregate(skip, 2, &keypairs, None, shred_version);
        entry
            .add_aggregate(aggregate, skip_validators.clone())
            .unwrap();

        let resp = build(entry, slot);
        assert!(resp.notar.is_none());
        assert_eq!(resp.skip.unwrap().slot, slot);
        assert_eq!(resp.validators, skip_validators);
    }

    /// Validators with equal stake voting in [`Self::SLOT`].
    struct CertificateFixture {
        /// The bank whose block carries the reward certs.
        bank: Arc<Bank>,
        rank_map: Arc<BLSPubkeyToRankMap>,
        keypairs: Vec<BlsKeypair>,
        shred_version: u16,
    }

    impl CertificateFixture {
        const SLOT: Slot = 123;

        fn new(num_validators: usize) -> Self {
            let (bank, rank_map, keypairs) = get_reward_bank(vec![100; num_validators], Self::SLOT);
            Self {
                bank,
                rank_map,
                keypairs,
                shred_version: rand::rng().random(),
            }
        }

        fn aggregate_signature(&self, vote: Vote, ranks: &[usize]) -> SignatureProjective {
            let payload = get_vote_payload_to_sign(vote, self.shred_version);
            let signatures = ranks
                .iter()
                .map(|&rank| self.keypairs[rank].sign(&payload))
                .collect::<Vec<_>>();
            SignatureProjective::aggregate(signatures.iter()).unwrap()
        }

        /// A vote aggregate that was verified as a whole.
        fn aggregate(&self, vote: Vote, ranks: &[usize]) -> (VoteAggregate, Vec<Pubkey>) {
            let aggregate = VoteAggregate::new_from_verified_votes(
                self.rank_map.len(),
                VotePayloadToSign::new_from_vote(vote, self.shred_version),
                ranks.iter().map(|&rank| {
                    let stake = self.rank_map.get_pubkey_stake_entry(rank).unwrap().stake;
                    (u16::try_from(rank).unwrap(), stake)
                }),
                self.aggregate_signature(vote, ranks),
            );
            (aggregate, self.vote_accounts(ranks))
        }

        /// The signers and signature of a certificate from another validator.
        fn certificate(&self, vote: Vote, ranks: &[usize]) -> (BitVec<u8>, BLSSignature) {
            let mut signers = BitVec::repeat(false, self.rank_map.len());
            for &rank in ranks {
                signers.set(rank, true);
            }
            (signers, self.aggregate_signature(vote, ranks).into())
        }

        fn vote_accounts(&self, ranks: &[usize]) -> Vec<Pubkey> {
            ranks
                .iter()
                .map(|&rank| {
                    self.rank_map
                        .get_pubkey_stake_entry(rank)
                        .unwrap()
                        .vote_account_pubkey
                })
                .collect()
        }

        /// Builds the reward certs, and returns the ranks of the validators they pay.
        fn build(&self, entry: Entry, stats: &mut CertificateStats) -> Vec<usize> {
            let resp = entry.build_certs(Self::SLOT, stats).unwrap();
            self.paid_ranks(resp)
        }

        /// Checks that replay accepts the reward certs and that they pay exactly the returned
        /// validators, and returns the ranks of those validators.
        fn paid_ranks(&self, resp: RewardRespSucc) -> Vec<usize> {
            assert!(
                ValidatedRewardCert::try_new(
                    &self.bank,
                    self.shred_version,
                    &resp.skip,
                    &resp.notar
                )
                .unwrap()
                .is_some()
            );
            let bitmaps = resp
                .skip
                .iter()
                .map(|skip| skip.to_bitmap())
                .chain(resp.notar.iter().map(|notar| notar.bitmap()));
            let mut signers = vec![];
            for bitmap in bitmaps {
                let Decoded::Base2(ranks) = decode(bitmap, self.rank_map.len()).unwrap() else {
                    panic!("reward certs are base2");
                };
                signers.extend(ranks.iter_ones());
            }
            signers.sort_unstable();
            let mut ranks = resp
                .validators
                .iter()
                .map(|pubkey| usize::from(*self.rank_map.get_rank_for_vote_pubkey(pubkey).unwrap()))
                .collect::<Vec<_>>();
            ranks.sort_unstable();
            assert_eq!(ranks, signers);
            ranks
        }
    }

    #[test]
    fn test_certificate_pays_votes_this_node_missed() {
        let fixture = CertificateFixture::new(6);
        for vote in [
            Vote::new_notarization_vote(Block::new_unique(CertificateFixture::SLOT)),
            Vote::new_skip_vote(CertificateFixture::SLOT),
        ] {
            let mut entry = Entry::new(fixture.rank_map.clone());
            for ranks in [&[0][..], &[4, 5]] {
                let (aggregate, validators) = fixture.aggregate(vote, ranks);
                entry.add_aggregate(aggregate, validators).unwrap();
            }
            // The votes of 1, 2 and 3 only reached this node inside another validator's cert.
            let (signers, signature) = fixture.certificate(vote, &[0, 1, 2, 3]);
            entry.add_certificate(vote, &signers, signature);

            let mut stats = CertificateStats::default();
            assert_eq!(fixture.build(entry, &mut stats), [0, 1, 2, 3, 4, 5]);
            assert_eq!(stats.validators_from_certs, 3);
        }
    }

    #[test]
    fn test_partly_overlapping_aggregate_keeps_collected_votes() {
        let fixture = CertificateFixture::new(6);
        let vote = Vote::new_notarization_vote(Block::new_unique(CertificateFixture::SLOT));
        let mut entry = Entry::new(fixture.rank_map.clone());
        let (aggregate, validators) = fixture.aggregate(vote, &[0, 4]);
        entry.add_aggregate(aggregate, validators).unwrap();
        // The aggregate cannot be split, and using the cert alone would drop validator 4.
        let (signers, signature) = fixture.certificate(vote, &[0, 1, 2, 3]);
        entry.add_certificate(vote, &signers, signature);

        let mut stats = CertificateStats::default();
        assert_eq!(fixture.build(entry, &mut stats), [0, 4]);
        assert_eq!(stats.validators_from_certs, 0);
        assert_eq!(stats.partially_overlapping_certs, 1);
    }

    #[test]
    fn test_certificate_that_adds_no_signers_keeps_collected_votes() {
        let fixture = CertificateFixture::new(6);
        let vote = Vote::new_skip_vote(CertificateFixture::SLOT);
        let mut entry = Entry::new(fixture.rank_map.clone());
        let (aggregate, validators) = fixture.aggregate(vote, &[0, 1, 2]);
        entry.add_aggregate(aggregate, validators).unwrap();
        let (signers, signature) = fixture.certificate(vote, &[0, 1]);
        entry.add_certificate(vote, &signers, signature);

        let mut stats = CertificateStats::default();
        assert_eq!(fixture.build(entry, &mut stats), [0, 1, 2]);
        assert_eq!(stats.validators_from_certs, 0);
        assert_eq!(stats.partially_overlapping_certs, 0);
    }

    #[test]
    fn test_certificate_without_collected_votes() {
        let fixture = CertificateFixture::new(6);
        let vote = Vote::new_notarization_vote(Block::new_unique(CertificateFixture::SLOT));
        let mut entry = Entry::new(fixture.rank_map.clone());
        let (signers, signature) = fixture.certificate(vote, &[1, 2, 3, 4]);
        entry.add_certificate(vote, &signers, signature);

        let mut stats = CertificateStats::default();
        assert_eq!(fixture.build(entry, &mut stats), [1, 2, 3, 4]);
        assert_eq!(stats.validators_from_certs, 4);
    }

    #[test]
    fn test_certificate_that_pays_the_most_stake_is_used() {
        let fixture = CertificateFixture::new(6);
        let vote = Vote::new_notarization_vote(Block::new_unique(CertificateFixture::SLOT));
        let mut entry = Entry::new(fixture.rank_map.clone());
        let (aggregate, validators) = fixture.aggregate(vote, &[0]);
        entry.add_aggregate(aggregate, validators).unwrap();
        // Overlapping certs cannot be combined with each other.
        for ranks in [&[0, 1][..], &[0, 2, 3]] {
            let (signers, signature) = fixture.certificate(vote, ranks);
            entry.add_certificate(vote, &signers, signature);
        }

        let mut stats = CertificateStats::default();
        assert_eq!(fixture.build(entry, &mut stats), [0, 2, 3]);
        assert_eq!(stats.validators_from_certs, 2);
    }

    #[test]
    fn test_certs_builder_uses_certificates() {
        let fixture = CertificateFixture::new(6);
        let keypair = Arc::new(Keypair::new());
        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&keypair.pubkey(), 0),
            keypair,
            SocketAddrSpace::Unspecified,
        ));
        let mut builder = CertsBuilder::new(cluster_info);
        let block = Block::new_unique(CertificateFixture::SLOT);
        let vote = Vote::new_notarization_vote(block);
        let (aggregate, _) = fixture.aggregate(vote, &[4, 5]);
        builder.handle_input(&fixture.bank, RewardInput::External(vec![aggregate]));
        // A cert's bitmap ends at its last signer.
        let (mut signers, signature) = fixture.certificate(vote, &[0, 1, 2, 3]);
        signers.truncate(4);
        builder.handle_input(
            &fixture.bank,
            RewardInput::Certificates(vec![Certificate {
                cert_type: CertificateType::Notarize(block),
                signature,
                bitmap: encode_base2(&signers).unwrap(),
            }]),
        );

        let resp = builder
            .build_certs(CertificateFixture::SLOT + NUM_SLOTS_FOR_REWARD)
            .unwrap();
        assert_eq!(fixture.paid_ranks(resp), [0, 1, 2, 3, 4, 5]);
    }
}
