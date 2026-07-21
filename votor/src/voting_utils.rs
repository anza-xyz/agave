use {
    crate::{
        commitment::{CommitmentAggregationData, CommitmentError},
        vote_history::{VoteHistory, VoteHistoryError},
        vote_history_storage::{SavedVoteHistory, SavedVoteHistoryVersions, VoteHistoryStorage},
        voting_service::BLSOp,
    },
    agave_bls_sigverify::rewards::RewardVoteMessage,
    agave_votor_messages::{
        consensus_message::{BLS_KEYPAIR_DERIVE_SEED, VoteMessage},
        metric_types::ConsensusMetricsEventSender,
        sig_verified_messages::SigVerifiedBatch,
        vote::Vote,
        wire::get_vote_payload_to_sign,
    },
    crossbeam_channel::{SendError, Sender, TrySendError},
    solana_bls_signatures::{BlsError, keypair::Keypair as BLSKeypair},
    solana_clock::{Epoch, Slot},
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks, epoch_stakes::BLSPubkeyStakeEntry},
    solana_signer::Signer,
    solana_transaction::Transaction,
    std::{
        collections::{HashMap, hash_map::Entry},
        num::NonZero,
        sync::{Arc, RwLock},
    },
    thiserror::Error,
};

#[derive(Debug)]
pub enum GenerateVoteTxResult {
    // The following are transient errors
    // non voting validator, not eligible for refresh
    // until authorized keypair is overridden
    NonVoting,
    // hot spare validator, not eligible for refresh
    // until set identity is invoked
    HotSpare,
    // The hash verification at startup has not completed
    WaitForStartupVerification,
    // Wait to vote slot is not reached
    WaitToVoteSlot(Slot),
    // no rank found, this can happen if the validator
    // is not staked in the current epoch, but it may
    // still be staked in future or past epochs, so this
    // is considered a transient error
    NoRankFound,

    // The following are misconfiguration errors
    // The authorized voter for the given pubkey and Epoch does not exist
    NoAuthorizedVoter(Pubkey, Epoch),
    // The vote account associated with given pubkey does not exist
    VoteAccountNotFound(Pubkey),

    // The following are the successful cases
    // Generated a vote transaction
    Tx(Transaction),
    // Generated a VoteMessage
    Vote(VoteMessage, NonZero<u64>, Pubkey),
}

impl GenerateVoteTxResult {
    pub fn is_non_voting(&self) -> bool {
        matches!(self, Self::NonVoting)
    }

    pub fn is_hot_spare(&self) -> bool {
        matches!(self, Self::HotSpare)
    }

    pub fn is_invalid_config(&self) -> bool {
        match self {
            Self::NoAuthorizedVoter(_, _) | Self::VoteAccountNotFound(_) => true,
            Self::NonVoting
            | Self::HotSpare
            | Self::WaitForStartupVerification
            | Self::WaitToVoteSlot(_)
            | Self::NoRankFound => false,
            Self::Tx(_) | Self::Vote(_, _, _) => false,
        }
    }

    pub fn is_transient_error(&self) -> bool {
        match self {
            Self::NoAuthorizedVoter(_, _) | Self::VoteAccountNotFound(_) => false,
            Self::NonVoting
            | Self::HotSpare
            | Self::WaitForStartupVerification
            | Self::WaitToVoteSlot(_)
            | Self::NoRankFound => true,
            Self::Tx(_) | Self::Vote(_, _, _) => false,
        }
    }
}

#[derive(Debug, Error)]
pub enum VoteError {
    #[error("Unable to generate bls vote message, transient error: {0:?}")]
    TransientError(Box<GenerateVoteTxResult>),

    #[error("Unable to generate bls vote message, configuration error: {0:?}")]
    InvalidConfig(Box<GenerateVoteTxResult>),

    #[error("Unable to send to certificate pool")]
    ConsensusPoolError(#[from] SendError<()>),

    #[error("Channel to rewards container disconnected")]
    RewardsChannelDisconnected,

    #[error("Commitment sender error {0}")]
    CommitmentSenderError(#[from] CommitmentError),

    #[error("Saved vote history error {0}")]
    SavedVoteHistoryError(#[from] VoteHistoryError),
}

/// Context required to construct vote transactions
pub struct VotingContext {
    pub cluster_info: Arc<ClusterInfo>,
    pub leader_schedule: Arc<LeaderScheduleCache>,
    pub vote_history: VoteHistory,
    pub vote_account_pubkey: Pubkey,
    pub identity_keypair: Arc<Keypair>,
    pub authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    pub vote_history_storage: Arc<dyn VoteHistoryStorage>,
    // The BLS keypair should always change with authorized_voter_keypairs.
    pub derived_bls_keypairs: HashMap<Pubkey, Arc<BLSKeypair>>,
    pub own_vote_sender: Sender<SigVerifiedBatch>,
    pub reward_votes_sender: Sender<Vec<RewardVoteMessage>>,
    pub bls_sender: Sender<BLSOp>,
    pub commitment_sender: Sender<CommitmentAggregationData>,
    pub wait_to_vote_slot: Option<u64>,
    pub sharable_banks: SharableBanks,
    pub consensus_metrics_sender: ConsensusMetricsEventSender,
}

fn get_or_insert_bls_keypair(
    derived_bls_keypairs: &mut HashMap<Pubkey, Arc<BLSKeypair>>,
    authorized_voter_keypair: &Keypair,
) -> Result<Arc<BLSKeypair>, BlsError> {
    let pubkey = authorized_voter_keypair.pubkey();
    match derived_bls_keypairs.entry(pubkey) {
        Entry::Occupied(e) => Ok(e.get().clone()),
        Entry::Vacant(e) => {
            let bls_keypair = Arc::new(BLSKeypair::derive_from_signer(
                authorized_voter_keypair,
                BLS_KEYPAIR_DERIVE_SEED,
            )?);
            e.insert(bls_keypair.clone());
            Ok(bls_keypair)
        }
    }
}

pub fn generate_vote_tx(
    vote: Vote,
    bank: &Bank,
    vote_account_pubkey: Pubkey,
    shred_version: u16,
    identity_keypair: &Keypair,
    authorized_voter_keypairs: &RwLock<Vec<Arc<Keypair>>>,
    wait_to_vote_slot: Option<u64>,
    derived_bls_keypairs: &mut HashMap<Pubkey, Arc<BLSKeypair>>,
) -> GenerateVoteTxResult {
    if authorized_voter_keypairs.read().unwrap().is_empty() {
        return GenerateVoteTxResult::NonVoting;
    }
    if bank.get_vote_account(&vote_account_pubkey).is_none() {
        return GenerateVoteTxResult::VoteAccountNotFound(vote_account_pubkey);
    }
    if let Some(slot) = wait_to_vote_slot
        && vote.slot() < slot
    {
        return GenerateVoteTxResult::WaitToVoteSlot(slot);
    }

    let rank_map = bank
        .get_rank_map(vote.slot())
        .unwrap_or_else(|| panic!("could not find rank map for slot {}", vote.slot()));

    let Some(&my_rank) = rank_map.get_rank_for_vote_pubkey(&vote_account_pubkey) else {
        return GenerateVoteTxResult::NoRankFound;
    };
    let BLSPubkeyStakeEntry {
        vote_account_pubkey: expected_vote_pubkey,
        node_pubkey: expected_node_pubkey,
        bls_pubkey: expected_bls_pubkey,
        stake,
    } = rank_map
        .get_pubkey_stake_entry(my_rank as usize)
        .expect("rank-map index should be valid");

    if expected_vote_pubkey != &vote_account_pubkey {
        warn!(
            "Rank-map vote pubkey mismatch: rank={my_rank}; expected={vote_account_pubkey}; \
             got={expected_vote_pubkey}",
        );
        return GenerateVoteTxResult::VoteAccountNotFound(vote_account_pubkey);
    }
    if expected_node_pubkey != &identity_keypair.pubkey() {
        warn!(
            "Rank-map node pubkey mismatch: rank={my_rank}; expected={expected_node_pubkey}; \
             got={}",
            identity_keypair.pubkey(),
        );
        return GenerateVoteTxResult::HotSpare;
    }

    let Some(bls_keypair) =
        authorized_voter_keypairs
            .read()
            .unwrap()
            .iter()
            .find_map(|authorized_voter_keypair| {
                let bls_keypair =
                    get_or_insert_bls_keypair(derived_bls_keypairs, authorized_voter_keypair)
                        .unwrap_or_else(|e| panic!("Failed to derive my own BLS keypair: {e}"));
                (&bls_keypair.public == expected_bls_pubkey).then_some(bls_keypair)
            })
    else {
        warn!(
            "No authorized voter keypair matches rank-map BLS key for vote account \
             {vote_account_pubkey}. Unable to vote"
        );
        return GenerateVoteTxResult::NonVoting;
    };

    let vote_payload_to_sign = get_vote_payload_to_sign(vote, shred_version);
    GenerateVoteTxResult::Vote(
        VoteMessage {
            vote,
            signature: bls_keypair.sign(&vote_payload_to_sign).into(),
            rank: my_rank,
        },
        *stake,
        *expected_vote_pubkey,
    )
}

/// Creates a vote message from `vote`, respecting `context.wait_to_vote_slot` only if `respect_wait_to_vote` is true
fn create_vote_message(
    vote: Vote,
    context: &mut VotingContext,
    respect_wait_to_vote: bool,
) -> Result<(VoteMessage, NonZero<u64>, Pubkey), VoteError> {
    let bank = context.sharable_banks.root();
    let wait_to_vote_slot = if respect_wait_to_vote {
        context.wait_to_vote_slot
    } else {
        None
    };
    match generate_vote_tx(
        vote,
        &bank,
        context.vote_account_pubkey,
        context.cluster_info.my_shred_version(),
        &context.identity_keypair,
        &context.authorized_voter_keypairs,
        wait_to_vote_slot,
        &mut context.derived_bls_keypairs,
    ) {
        GenerateVoteTxResult::Vote(vote, stake, vote_account_pubkey) => {
            Ok((vote, stake, vote_account_pubkey))
        }
        e => {
            if e.is_transient_error() {
                Err(VoteError::TransientError(Box::new(e)))
            } else {
                Err(VoteError::InvalidConfig(Box::new(e)))
            }
        }
    }
}

fn handle_skippable_vote_error(err: VoteError, action: &str) -> Result<(), VoteError> {
    match err {
        VoteError::InvalidConfig(e) => {
            warn!("Failed to {action}: {e:?}");
            // These are not fatal errors, just skip the vote for now. But they are
            // misconfigurations that should be warned about.
            Ok(())
        }
        VoteError::TransientError(e) => {
            info!("Failed to {action}: {e:?}");
            // These are transient errors, just skip the vote for now.
            Ok(())
        }
        e => Err(e),
    }
}

/// Build a normal push-vote BLS op.
///
/// This updates vote history, sends the vote to the certificate pool and reward service for
/// ingestion, and saves vote history for persistence.
pub fn insert_vote_and_create_bls_message(
    vote: Vote,
    context: &mut VotingContext,
) -> Result<Option<BLSOp>, VoteError> {
    // Update and save the vote history
    context.vote_history.add_vote(vote);

    let Some(vote_msg) =
        create_and_send_own_vote_message(vote, context, /* respect_wait_to_vote */ true)?
    else {
        return Ok(None);
    };

    let saved_vote_history =
        SavedVoteHistory::new(&context.vote_history, &context.identity_keypair)?;
    context
        .vote_history_storage
        .store(&SavedVoteHistoryVersions::from(saved_vote_history))?;

    // Return vote for sending
    Ok(Some(BLSOp::PushVote {
        vote: Arc::new(vote_msg),
    }))
}

pub(crate) fn create_and_send_own_vote_message(
    vote: Vote,
    context: &mut VotingContext,
    respect_wait_to_vote: bool,
) -> Result<Option<VoteMessage>, VoteError> {
    let (vote_msg, stake, vote_account_pubkey) =
        match create_vote_message(vote, context, respect_wait_to_vote) {
            Ok(vote_msg) => vote_msg,
            Err(e) => {
                handle_skippable_vote_error(e, "generate vote message")?;
                return Ok(None);
            }
        };

    context
        .own_vote_sender
        .send(SigVerifiedBatch::Votes(vec![vote_msg.clone()]))
        .map_err(|_| SendError(()))?;

    let root_slot = context.sharable_banks.root().slot();
    if let Some(reward_vote_msg) = RewardVoteMessage::try_new(
        &context.cluster_info,
        &context.leader_schedule,
        root_slot,
        &vote_msg,
        stake,
        vote_account_pubkey,
    ) {
        match context.reward_votes_sender.try_send(vec![reward_vote_msg]) {
            Ok(()) => (),
            Err(TrySendError::Full(_)) => {
                warn!("Reward votes channel is full, dropping own vote {vote:?}");
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(VoteError::RewardsChannelDisconnected);
            }
        }
    }

    Ok(Some(vote_msg))
}

pub fn generate_refresh_vote_message(
    vote: Vote,
    vctx: &mut VotingContext,
) -> Result<Option<VoteMessage>, VoteError> {
    match create_vote_message(vote, vctx, /* respect_wait_to_vote */ true) {
        Ok((vote_msg, _, _)) => Ok(Some(vote_msg)),
        Err(e) => {
            handle_skippable_vote_error(e, "generate refresh vote message")?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::vote_history_storage::NullVoteHistoryStorage,
        agave_votor_messages::consensus_message::Block,
        crossbeam_channel::{Receiver, bounded},
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_net_utils::SocketAddrSpace,
        solana_runtime::{
            bank::{Bank, SlotLeader},
            bank_forks::BankForks,
            epoch_stakes::VersionedEpochStakes,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        std::sync::{Arc, RwLock},
    };

    fn generate_expected_consensus_message(
        ctx: &VotingContext,
        vote: Vote,
        my_bls_keypair: &BLSKeypair,
    ) -> VoteMessage {
        let payload = get_vote_payload_to_sign(vote, ctx.cluster_info.my_shred_version());
        let signature = my_bls_keypair.sign(&payload);
        VoteMessage {
            vote,
            signature: signature.into(),
            rank: 0,
        }
    }

    fn setup_voting_context_and_bank_forks(
        own_vote_sender: Sender<SigVerifiedBatch>,
        validator_keypairs: &[ValidatorVoteKeypairs],
        my_index: usize,
    ) -> (VotingContext, Receiver<Vec<RewardVoteMessage>>) {
        let (voting_context, _, reward_votes_receiver) =
            setup_voting_context_and_bank_forks_with_forks(
                own_vote_sender,
                validator_keypairs,
                my_index,
            );
        (voting_context, reward_votes_receiver)
    }

    fn setup_voting_context_and_bank_forks_with_forks(
        own_vote_sender: Sender<SigVerifiedBatch>,
        validator_keypairs: &[ValidatorVoteKeypairs],
        my_index: usize,
    ) -> (
        VotingContext,
        Arc<RwLock<BankForks>>,
        Receiver<Vec<RewardVoteMessage>>,
    ) {
        // Can't have stake of 0, so start at 1 and go to 10. In descending order, so 0 has largest stake.
        let stakes: Vec<u64> = (1u64..=10).rev().map(|x| x.saturating_mul(100)).collect();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            validator_keypairs,
            stakes,
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank0);

        let my_keys = &validator_keypairs[my_index];
        let contact_info = ContactInfo::new_localhost(&my_keys.node_keypair.pubkey(), 0);
        let cluster_info = Arc::new(ClusterInfo::new(
            contact_info,
            Arc::new(my_keys.node_keypair.insecure_clone()),
            SocketAddrSpace::Unspecified,
        ));
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));
        let bls_sender = bounded(1024).0;
        let commitment_sender = bounded(1024).0;
        let consensus_metrics_sender = bounded(1024).0;
        let (reward_votes_sender, reward_votes_receiver) = bounded(1024);
        let voting_context = VotingContext {
            cluster_info,
            vote_history: VoteHistory::new(my_keys.node_keypair.pubkey(), 0),
            vote_account_pubkey: my_keys.vote_keypair.pubkey(),
            identity_keypair: Arc::new(my_keys.node_keypair.insecure_clone()),
            authorized_voter_keypairs: Arc::new(RwLock::new(vec![Arc::new(
                my_keys.vote_keypair.insecure_clone(),
            )])),
            vote_history_storage: Arc::new(NullVoteHistoryStorage::default()),
            derived_bls_keypairs: HashMap::new(),
            own_vote_sender,
            reward_votes_sender,
            bls_sender,
            commitment_sender,
            wait_to_vote_slot: None,
            sharable_banks,
            consensus_metrics_sender,
            leader_schedule,
        };
        (voting_context, bank_forks, reward_votes_receiver)
    }

    #[test]
    fn test_generate_own_vote_message() {
        let (own_vote_sender, own_vote_receiver) = bounded(1024);
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let (mut voting_context, reward_votes_receiver) =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);
        let my_bls_keypair = BLSKeypair::derive_from_signer(
            &validator_keypairs[my_index].vote_keypair,
            BLS_KEYPAIR_DERIVE_SEED,
        )
        .unwrap();

        // Generate a normal notarization vote and check it's sent out correctly.
        let block_id = Hash::new_unique();
        let vote_slot = 2;
        let vote = Vote::new_notarization_vote(Block {
            slot: vote_slot,
            block_id,
        });
        let result = insert_vote_and_create_bls_message(vote, &mut voting_context)
            .ok()
            .unwrap()
            .unwrap();
        let expected_message =
            generate_expected_consensus_message(&voting_context, vote, &my_bls_keypair);
        if let BLSOp::PushVote { vote } = result {
            let msg = Arc::unwrap_or_clone(vote);
            assert_eq!(msg, expected_message);
        } else {
            panic!("Expected BLSOp::VotePush, got {result:?}");
        }

        // Check that own vote sender receives the vote
        let received_message = own_vote_receiver.recv().unwrap();
        assert_eq!(
            received_message,
            SigVerifiedBatch::Votes(vec![expected_message.clone()])
        );

        // Check that the reward service receives the vote.
        let reward_votes = reward_votes_receiver.recv().unwrap();
        assert_eq!(reward_votes.len(), 1);
        let received_vote_msg = VoteMessage::from(reward_votes[0].clone());
        assert_eq!(received_vote_msg, expected_message);

        let refresh_vote = Vote::new_notarization_vote(Block {
            slot: vote_slot,
            block_id,
        });
        let refresh_result = generate_refresh_vote_message(refresh_vote, &mut voting_context)
            .ok()
            .unwrap()
            .unwrap();
        assert_eq!(refresh_result.vote.slot(), vote_slot);
        assert_eq!(refresh_result, expected_message);
        assert!(own_vote_receiver.try_recv().is_err());
        assert!(reward_votes_receiver.try_recv().is_err());
    }

    #[test]
    fn test_wait_to_vote_slot() {
        let (own_vote_sender, _own_vote_receiver) = bounded(1024);
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let (mut voting_context, _reward_votes_receiver) =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // If we haven't reached wait_to_vote_slot yet, return Ok(None)
        voting_context.wait_to_vote_slot = Some(4);
        let vote = Vote::new_finalization_vote(2);
        assert!(
            insert_vote_and_create_bls_message(vote, &mut voting_context)
                .unwrap()
                .is_none()
        );

        // If we have reached wait_to_vote_slot, we should be able to vote
        voting_context.wait_to_vote_slot = Some(1);
        assert!(
            insert_vote_and_create_bls_message(vote, &mut voting_context)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_non_voting_node() {
        let (own_vote_sender, _own_vote_receiver) = bounded(1024);
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let (mut voting_context, _reward_votes_receiver) =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // Empty authorized voter keypairs to simulate non voting node
        voting_context.authorized_voter_keypairs = Arc::new(std::sync::RwLock::new(vec![]));
        let vote = Vote::new_skip_vote(5);
        assert!(matches!(
            generate_vote_tx(
                vote,
                &voting_context.sharable_banks.root(),
                voting_context.vote_account_pubkey,
                voting_context.cluster_info.my_shred_version(),
                &voting_context.identity_keypair,
                &voting_context.authorized_voter_keypairs,
                voting_context.wait_to_vote_slot,
                &mut voting_context.derived_bls_keypairs,
            ),
            GenerateVoteTxResult::NonVoting
        ));

        // Recover correct value to vote again
        voting_context.authorized_voter_keypairs = Arc::new(RwLock::new(vec![Arc::new(
            validator_keypairs[my_index].vote_keypair.insecure_clone(),
        )]));
        assert!(
            insert_vote_and_create_bls_message(vote, &mut voting_context)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_wrong_identity_keypair() {
        let (own_vote_sender, _own_vote_receiver) = bounded(1024);
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let (mut voting_context, _reward_votes_receiver) =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // Wrong identity keypair should return HotSpare based on rank_map.node_pubkey.
        let wrong_identity_keypair = Arc::new(Keypair::new());
        let vote = Vote::new_notarization_vote(Block {
            slot: 6,
            block_id: Hash::new_unique(),
        });
        assert!(matches!(
            generate_vote_tx(
                vote,
                &voting_context.sharable_banks.root(),
                voting_context.vote_account_pubkey,
                voting_context.cluster_info.my_shred_version(),
                &wrong_identity_keypair,
                &voting_context.authorized_voter_keypairs,
                voting_context.wait_to_vote_slot,
                &mut voting_context.derived_bls_keypairs,
            ),
            GenerateVoteTxResult::HotSpare
        ));
    }

    #[test]
    fn test_wrong_vote_account_pubkey() {
        let (own_vote_sender, _own_vote_receiver) = bounded(1024);
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let (mut voting_context, _reward_votes_receiver) =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // Wrong vote account pubkey
        voting_context.vote_account_pubkey = Pubkey::new_unique();
        let vote = Vote::new_notarization_vote(Block {
            slot: 7,
            block_id: Hash::new_unique(),
        });
        assert!(
            generate_refresh_vote_message(vote, &mut voting_context)
                .unwrap()
                .is_none()
        );

        // Recover correct value to vote again
        voting_context.vote_account_pubkey = validator_keypairs[my_index].vote_keypair.pubkey();
        assert!(
            generate_refresh_vote_message(vote, &mut voting_context)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    #[should_panic(expected = "could not find rank map for slot 1000000000")]
    fn test_panic_on_future_slot() {
        agave_logger::setup();
        let (own_vote_sender, _own_vote_receiver) = bounded(1024);
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let (mut voting_context, _reward_votes_receiver) =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // If we try to vote for a slot in the future, we should panic
        let vote = Vote::new_notarization_vote(Block {
            slot: 1_000_000_000,
            block_id: Hash::new_unique(),
        });
        let _ = insert_vote_and_create_bls_message(vote, &mut voting_context);
    }

    #[test]
    fn test_zero_staked_validator_fails_voting() {
        agave_logger::setup();
        let (own_vote_sender, _own_vote_receiver) = bounded(1024);
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let (mut voting_context, bank_forks, _reward_votes_receiver) =
            setup_voting_context_and_bank_forks_with_forks(
                own_vote_sender,
                &validator_keypairs,
                my_index,
            );

        // Set the stake of my_index to 0 in epoch 2
        // For epoch 2, make validator my_index to be zero stake, others have stake in ascending order, 1 < 2 < ... < 9
        let bank = voting_context.sharable_banks.root();
        assert_eq!(bank.epoch(), 0);
        assert!(bank.epoch_stakes(2).is_none());
        let vote_accounts_hash_map = validator_keypairs
            .iter()
            .enumerate()
            .map(|(i, keypairs)| {
                let stake = if i == my_index {
                    0
                } else {
                    i.saturating_mul(100)
                };
                let authorized_voter = keypairs.vote_keypair.pubkey();
                // Read vote_account from bank 0
                let vote_account = bank.get_vote_account(&authorized_voter).unwrap();
                (authorized_voter, (stake as u64, vote_account))
            })
            .collect();
        let mut new_bank = Bank::new_from_parent(bank, SlotLeader::default(), 1);
        assert!(new_bank.epoch_stakes(2).is_none());
        let epoch2_epoch_stakes = VersionedEpochStakes::new_for_tests(vote_accounts_hash_map, 2);
        new_bank.set_epoch_stakes_for_test(2, epoch2_epoch_stakes);
        assert!(new_bank.epoch_stakes(2).is_some());
        new_bank.freeze();
        bank_forks.write().unwrap().insert(new_bank);
        bank_forks.write().unwrap().set_root(1, None, None);
        voting_context.sharable_banks = bank_forks.read().unwrap().sharable_banks();

        // If we try to vote for a slot in epoch 1, it should succeed
        let first_slot_in_epoch_1 = voting_context
            .sharable_banks
            .root()
            .epoch_schedule()
            .get_first_slot_in_epoch(1);
        let vote = Vote::new_notarization_vote(Block {
            slot: first_slot_in_epoch_1,
            block_id: Hash::new_unique(),
        });
        assert!(
            insert_vote_and_create_bls_message(vote, &mut voting_context)
                .unwrap()
                .is_some()
        );

        // If we try to vote for a slot in epoch 2, we should get NoRankFound error
        let first_slot_in_epoch_2 = voting_context
            .sharable_banks
            .root()
            .epoch_schedule()
            .get_first_slot_in_epoch(2);
        let vote = Vote::new_notarization_vote(Block {
            slot: first_slot_in_epoch_2,
            block_id: Hash::new_unique(),
        });
        assert!(
            insert_vote_and_create_bls_message(vote, &mut voting_context)
                .unwrap()
                .is_none()
        );
    }
}
