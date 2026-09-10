use {
    crate::votes_verifier::UnverifiedVote,
    agave_bls_cert_verify::cert_verify::test_create_base2_unverified_certificate,
    agave_votor_messages::{
        certificate::CertificateType,
        consensus_message::{ConsensusMessage, VoteMessage},
        unverified_vote_message::{UnverifiedCertificate, UnverifiedVoteMessage},
        vote::Vote,
        wire::{VersionedWireConsensusMessage, get_vote_payload_to_sign},
    },
    agave_votor_transport::endpoint::Datagram,
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::Keypair,
    solana_net_utils::SocketAddrSpace,
    solana_runtime::{
        bank::Bank,
        bank_forks::{BankForks, SharableBanks},
        genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
    },
    solana_signer::Signer,
    std::sync::{Arc, RwLock},
};

pub(crate) struct MetricsTestContext {
    pub validators: Vec<ValidatorVoteKeypairs>,
    pub banks: SharableBanks,
    pub cluster_info: Arc<ClusterInfo>,
    // Banks retain a weak reference to the fork graph when creating child banks.
    _forks: Arc<RwLock<BankForks>>,
}

impl MetricsTestContext {
    pub fn new() -> Self {
        let validators = (0..4)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let mut genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validators,
            vec![400, 300, 200, 100],
        );
        genesis.genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        let forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
        let banks = forks.read().unwrap().sharable_banks();
        let keypair = Arc::new(Keypair::new());
        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&keypair.pubkey(), 0),
            keypair,
            SocketAddrSpace::Unspecified,
        ));
        Self {
            validators,
            banks,
            cluster_info,
            _forks: forks,
        }
    }

    pub fn vote(&self, vote: Vote, rank: usize) -> UnverifiedVote {
        let map = self.banks.root().get_rank_map(0).unwrap().clone();
        let entry = map.get_pubkey_stake_entry(rank).unwrap();
        let shred_version = self.cluster_info.my_shred_version();
        let signature = self.validators[rank]
            .bls_keypair
            .sign(&get_vote_payload_to_sign(vote, shred_version));
        UnverifiedVote {
            vote_message: UnverifiedVoteMessage {
                vote,
                signature: signature.into(),
                shred_version,
            },
            sender_bls_pubkey: entry.bls_pubkey,
            sender_vote_account_pubkey: entry.vote_account_pubkey,
            sender_identity_pubkey: self.validators[rank].node_keypair.pubkey(),
            rank: rank as u16,
            stake: entry.stake,
        }
    }

    pub fn datagram(&self, vote: Vote, rank: usize) -> Datagram {
        let vote = self.vote(vote, rank);
        let message = ConsensusMessage::Vote(VoteMessage {
            vote: vote.vote_message.vote,
            signature: vote.vote_message.signature.try_into().unwrap(),
            rank: vote.rank,
            stake: vote.stake,
        });
        Datagram {
            peer_pubkey: vote.sender_identity_pubkey,
            peer_address: "127.0.0.1:1".parse().unwrap(),
            message: wincode::serialize(&VersionedWireConsensusMessage::new(
                message,
                vote.vote_message.shred_version,
            ))
            .unwrap()
            .into(),
        }
    }

    pub fn certificate(&self, cert_type: CertificateType) -> UnverifiedCertificate {
        let keys = self
            .validators
            .iter()
            .map(|v| v.bls_keypair.clone())
            .collect::<Vec<_>>();
        test_create_base2_unverified_certificate(
            &keys,
            self.cluster_info.my_shred_version(),
            cert_type,
            &[0, 1, 2, 3],
        )
    }
}
