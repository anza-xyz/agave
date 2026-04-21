use {
    crate::{
        commitment::{CommitmentAggregationData, CommitmentError},
        consensus_metrics::ConsensusMetricsEventSender,
        vote_history::{VoteHistory, VoteHistoryError},
        vote_history_storage::{SavedVoteHistory, SavedVoteHistoryVersions},
        voting_service::BLSOp,
    },
    agave_votor_messages::{
        consensus_message::{BLS_KEYPAIR_DERIVE_SEED, ConsensusMessage, VoteMessage},
        vote::Vote,
    },
    crossbeam_channel::{SendError, Sender},
    solana_bls_signatures::{BlsError, keypair::Keypair as BLSKeypair},
    solana_clock::{Epoch, Slot},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks, epoch_stakes::BLSPubkeyStakeEntry},
    solana_signer::Signer,
    solana_transaction::Transaction,
    std::{
        collections::{HashMap, hash_map::Entry},
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
    // Generated a ConsensusMessage
    ConsensusMessage(ConsensusMessage),
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
            Self::Tx(_) | Self::ConsensusMessage(_) => false,
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
            Self::Tx(_) | Self::ConsensusMessage(_) => false,
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

    #[error("Commitment sender error {0}")]
    CommitmentSenderError(#[from] CommitmentError),

    #[error("Saved vote history error {0}")]
    SavedVoteHistoryError(#[from] VoteHistoryError),
}

/// Context required to construct vote transactions
pub struct VotingContext {
    pub vote_history: VoteHistory,
    pub vote_account_pubkey: Pubkey,
    pub identity_keypair: Arc<Keypair>,
    pub authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    // The BLS keypair should always change with authorized_voter_keypairs.
    pub derived_bls_keypairs: HashMap<Pubkey, Arc<BLSKeypair>>,
    pub own_vote_sender: Sender<Vec<ConsensusMessage>>,
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
    identity_keypair: &Keypair,
    authorized_voter_keypairs: &RwLock<Vec<Arc<Keypair>>>,
    wait_to_vote_slot: Option<u64>,
    derived_bls_keypairs: &mut HashMap<Pubkey, Arc<BLSKeypair>>,
) -> GenerateVoteTxResult {
    if bank.get_vote_account(&vote_account_pubkey).is_none() {
        return GenerateVoteTxResult::VoteAccountNotFound(vote_account_pubkey);
    }
    if let Some(slot) = wait_to_vote_slot {
        if vote.slot() < slot {
            return GenerateVoteTxResult::WaitToVoteSlot(slot);
        }
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
        stake: _,
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

    let vote_serialized = wincode::serialize(&vote).unwrap();
    GenerateVoteTxResult::ConsensusMessage(ConsensusMessage::Vote(VoteMessage {
        vote,
        signature: bls_keypair.sign(&vote_serialized).into(),
        rank: my_rank,
    }))
}

/// Send an alpenglow vote as a BLSMessage
/// `bank` will be used for:
/// - startup verification
/// - vote account checks
/// - authorized voter checks
///
/// We also update the vote history and send the vote to
/// the certificate pool thread for ingestion.
///
/// Returns false if we are currently a non-voting node
fn insert_vote_and_create_bls_message(
    vote: Vote,
    is_refresh: bool,
    context: &mut VotingContext,
) -> Result<BLSOp, VoteError> {
    // Update and save the vote history
    if !is_refresh {
        context.vote_history.add_vote(vote);
    }

    let bank = context.sharable_banks.root();
    let message = match generate_vote_tx(
        vote,
        &bank,
        context.vote_account_pubkey,
        &context.identity_keypair,
        &context.authorized_voter_keypairs,
        context.wait_to_vote_slot,
        &mut context.derived_bls_keypairs,
    ) {
        GenerateVoteTxResult::ConsensusMessage(m) => m,
        e => {
            if e.is_transient_error() {
                return Err(VoteError::TransientError(Box::new(e)));
            } else {
                return Err(VoteError::InvalidConfig(Box::new(e)));
            }
        }
    };
    context
        .own_vote_sender
        .send(vec![message.clone()])
        .map_err(|_| SendError(()))?;

    // TODO: for refresh votes use a different BLSOp so we don't have to rewrite the same vote history to file
    let saved_vote_history =
        SavedVoteHistory::new(&context.vote_history, &context.identity_keypair)?;

    // Return vote for sending
    Ok(BLSOp::PushVote {
        message: Arc::new(message),
        slot: vote.slot(),
        saved_vote_history: SavedVoteHistoryVersions::from(saved_vote_history),
    })
}

pub fn generate_vote_message(
    vote: Vote,
    is_refresh: bool,
    vctx: &mut VotingContext,
) -> Result<Option<BLSOp>, VoteError> {
    let bls_op = match insert_vote_and_create_bls_message(vote, is_refresh, vctx) {
        Ok(bls_op) => bls_op,
        Err(VoteError::InvalidConfig(e)) => {
            warn!("Failed to generate vote and push to votes: {e:?}");
            // These are not fatal errors, just skip the vote for now. But they are misconfigurations
            // that should be warned about.
            return Ok(None);
        }
        Err(VoteError::TransientError(e)) => {
            info!("Failed to generate vote and push to votes: {e:?}");
            // These are transient errors, just skip the vote for now.
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    Ok(Some(bls_op))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::unbounded,
        solana_hash::Hash,
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
        vote: Vote,
        my_bls_keypair: &BLSKeypair,
    ) -> ConsensusMessage {
        let vote_serialized = bincode::serialize(&vote).unwrap();
        let signature = my_bls_keypair.sign(&vote_serialized);
        ConsensusMessage::Vote(VoteMessage {
            vote,
            signature: signature.into(),
            rank: 0,
        })
    }

    fn setup_voting_context_and_bank_forks(
        own_vote_sender: Sender<Vec<ConsensusMessage>>,
        validator_keypairs: &[ValidatorVoteKeypairs],
        my_index: usize,
    ) -> VotingContext {
        let (voting_context, _) = setup_voting_context_and_bank_forks_with_forks(
            own_vote_sender,
            validator_keypairs,
            my_index,
        );
        voting_context
    }

    fn setup_voting_context_and_bank_forks_with_forks(
        own_vote_sender: Sender<Vec<ConsensusMessage>>,
        validator_keypairs: &[ValidatorVoteKeypairs],
        my_index: usize,
    ) -> (VotingContext, Arc<RwLock<BankForks>>) {
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
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let bls_sender = unbounded().0;
        let commitment_sender = unbounded().0;
        let consensus_metrics_sender = unbounded().0;
        let voting_context = VotingContext {
            vote_history: VoteHistory::new(my_keys.node_keypair.pubkey(), 0),
            vote_account_pubkey: my_keys.vote_keypair.pubkey(),
            identity_keypair: Arc::new(my_keys.node_keypair.insecure_clone()),
            authorized_voter_keypairs: Arc::new(RwLock::new(vec![Arc::new(
                my_keys.vote_keypair.insecure_clone(),
            )])),
            derived_bls_keypairs: HashMap::new(),
            own_vote_sender,
            bls_sender,
            commitment_sender,
            wait_to_vote_slot: None,
            sharable_banks,
            consensus_metrics_sender,
        };
        (voting_context, bank_forks)
    }

    #[test]
    fn test_generate_own_vote_message() {
        let (own_vote_sender, own_vote_receiver) = crossbeam_channel::unbounded();
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let mut voting_context =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);
        let my_bls_keypair = BLSKeypair::derive_from_signer(
            &validator_keypairs[my_index].vote_keypair,
            BLS_KEYPAIR_DERIVE_SEED,
        )
        .unwrap();

        // Generate a normal notarization vote and check it's sent out correctly.
        let block_id = Hash::new_unique();
        let vote_slot = 2;
        let vote = Vote::new_notarization_vote(vote_slot, block_id);
        let result = generate_vote_message(vote, false, &mut voting_context)
            .ok()
            .unwrap()
            .unwrap();
        let expected_message = generate_expected_consensus_message(vote, &my_bls_keypair);
        if let BLSOp::PushVote {
            message,
            slot,
            saved_vote_history,
        } = &result
        {
            assert_eq!(slot, &vote_slot);
            assert_eq!(**message, expected_message);
            assert_eq!(
                saved_vote_history,
                &SavedVoteHistoryVersions::from(
                    SavedVoteHistory::new(
                        &voting_context.vote_history,
                        &voting_context.identity_keypair
                    )
                    .unwrap()
                )
            );
        } else {
            panic!("Expected BLSOp::PushVote, got {result:?}");
        }

        // Check that own vote sender receives the vote
        let received_message = own_vote_receiver.recv().unwrap();
        assert_eq!(received_message, vec![expected_message]);
    }

    #[test]
    fn test_wait_to_vote_slot() {
        let (own_vote_sender, _own_vote_receiver) = crossbeam_channel::unbounded();
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let mut voting_context =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // If we haven't reached wait_to_vote_slot yet, return Ok(None)
        voting_context.wait_to_vote_slot = Some(4);
        let vote = Vote::new_finalization_vote(2);
        assert!(
            generate_vote_message(vote, false, &mut voting_context)
                .unwrap()
                .is_none()
        );

        // If we have reached wait_to_vote_slot, we should be able to vote
        voting_context.wait_to_vote_slot = Some(1);
        assert!(
            generate_vote_message(vote, false, &mut voting_context)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_non_voting_node() {
        let (own_vote_sender, _own_vote_receiver) = crossbeam_channel::unbounded();
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let mut voting_context =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // Empty authorized voter keypairs to simulate non voting node
        voting_context.authorized_voter_keypairs = Arc::new(std::sync::RwLock::new(vec![]));
        let vote = Vote::new_skip_vote(5);
        // For non-voting nodes, we just return Ok(None)
        assert!(
            generate_vote_message(vote, false, &mut voting_context)
                .unwrap()
                .is_none()
        );

        // Recover correct value to vote again
        voting_context.authorized_voter_keypairs = Arc::new(RwLock::new(vec![Arc::new(
            validator_keypairs[my_index].vote_keypair.insecure_clone(),
        )]));
        assert!(
            generate_vote_message(vote, false, &mut voting_context)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_wrong_identity_keypair() {
        let (own_vote_sender, _own_vote_receiver) = crossbeam_channel::unbounded();
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let mut voting_context =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // Wrong identity keypair
        voting_context.identity_keypair = Arc::new(Keypair::new());
        let vote = Vote::new_notarization_vote(6, Hash::new_unique());
        assert!(
            generate_vote_message(vote, true, &mut voting_context)
                .unwrap()
                .is_none()
        );

        // Recover correct value to vote again
        voting_context.identity_keypair =
            Arc::new(validator_keypairs[my_index].node_keypair.insecure_clone());
        assert!(
            generate_vote_message(vote, true, &mut voting_context)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_wrong_vote_account_pubkey() {
        let (own_vote_sender, _own_vote_receiver) = crossbeam_channel::unbounded();
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let mut voting_context =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // Wrong vote account pubkey
        voting_context.vote_account_pubkey = Pubkey::new_unique();
        let vote = Vote::new_notarization_vote(7, Hash::new_unique());
        assert!(
            generate_vote_message(vote, true, &mut voting_context)
                .unwrap()
                .is_none()
        );

        // Recover correct value to vote again
        voting_context.vote_account_pubkey = validator_keypairs[my_index].vote_keypair.pubkey();
        assert!(
            generate_vote_message(vote, true, &mut voting_context)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    #[should_panic(expected = "could not find rank map for slot 1000000000")]
    fn test_panic_on_future_slot() {
        agave_logger::setup();
        let (own_vote_sender, _own_vote_receiver) = crossbeam_channel::unbounded();
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let mut voting_context =
            setup_voting_context_and_bank_forks(own_vote_sender, &validator_keypairs, my_index);

        // If we try to vote for a slot in the future, we should panic
        let vote = Vote::new_notarization_vote(1_000_000_000, Hash::new_unique());
        let _ = generate_vote_message(vote, false, &mut voting_context);
    }

    #[test]
    fn test_zero_staked_validator_fails_voting() {
        agave_logger::setup();
        let (own_vote_sender, _own_vote_receiver) = crossbeam_channel::unbounded();
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>();
        let my_index = 0;
        let (mut voting_context, bank_forks) = setup_voting_context_and_bank_forks_with_forks(
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
        let vote = Vote::new_notarization_vote(first_slot_in_epoch_1, Hash::new_unique());
        assert!(
            generate_vote_message(vote, false, &mut voting_context)
                .unwrap()
                .is_some()
        );

        // If we try to vote for a slot in epoch 2, we should get NoRankFound error
        let first_slot_in_epoch_2 = voting_context
            .sharable_banks
            .root()
            .epoch_schedule()
            .get_first_slot_in_epoch(2);
        let vote = Vote::new_notarization_vote(first_slot_in_epoch_2, Hash::new_unique());
        assert!(
            generate_vote_message(vote, false, &mut voting_context)
                .unwrap()
                .is_none()
        );
    }
}
