use {
    crate::{
        commitment::{AlpenglowCommitmentAggregationData, AlpenglowCommitmentError},
        vote_history::{VoteHistory, VoteHistoryError},
        vote_history_storage::{SavedVoteHistory, SavedVoteHistoryVersions},
    },
    crossbeam_channel::{SendError, Sender},
    solana_bls_signatures::{keypair::Keypair as BLSKeypair, BlsError, Pubkey as BLSPubkey},
    solana_clock::Slot,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBank},
    solana_signer::Signer,
    solana_transaction::Transaction,
    solana_votor_messages::{
        consensus_message::{
            CertificateMessage, ConsensusMessage, VoteMessage, BLS_KEYPAIR_DERIVE_SEED,
        },
        vote::Vote,
    },
    std::{collections::HashMap, sync::Arc},
    thiserror::Error,
};

#[derive(Debug)]
pub enum GenerateVoteTxResult {
    // The following are transient errors
    // non voting validator, not eligible for refresh
    // until authorized keypair is overriden
    NonVoting,
    // hot spare validator, not eligble for refresh
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
    NoAuthorizedVoter(Pubkey, u64),
    // The vote state associated with given pubkey does not exist
    NoVoteState(Pubkey),
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
            Self::NoAuthorizedVoter(_, _) | Self::NoVoteState(_) | Self::VoteAccountNotFound(_) => {
                true
            }
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
            Self::NoAuthorizedVoter(_, _) | Self::NoVoteState(_) | Self::VoteAccountNotFound(_) => {
                false
            }
            Self::NonVoting
            | Self::HotSpare
            | Self::WaitForStartupVerification
            | Self::WaitToVoteSlot(_)
            | Self::NoRankFound => true,
            Self::Tx(_) | Self::ConsensusMessage(_) => false,
        }
    }
}

#[derive(Debug)]
pub enum BLSOp {
    PushVote {
        message: Arc<ConsensusMessage>,
        slot: Slot,
        saved_vote_history: SavedVoteHistoryVersions,
    },
    PushCertificate {
        certificate: Arc<CertificateMessage>,
    },
}

#[derive(Debug, Error)]
pub enum VoteError {
    #[error("Unable to generate bls vote message, transient error: {0:?}")]
    TransientError(Box<GenerateVoteTxResult>),

    #[error("Unable to generate bls vote message, configuration error: {0:?}")]
    InvalidConfig(Box<GenerateVoteTxResult>),

    #[error("Unable to send to certificate pool")]
    CertificatePoolError(#[from] SendError<()>),

    #[error("Commitment sender error {0}")]
    CommitmentSenderError(#[from] AlpenglowCommitmentError),

    #[error("Saved vote history error {0}")]
    SavedVoteHistoryError(#[from] VoteHistoryError),
}

/// Context required to construct vote transactions
pub struct VotingContext {
    pub vote_history: VoteHistory,
    pub vote_account_pubkey: Pubkey,
    pub identity_keypair: Arc<Keypair>,
    pub authorized_voter_keypairs: Arc<std::sync::RwLock<Vec<Arc<Keypair>>>>,
    // The BLS keypair should always change with authorized_voter_keypairs.
    pub derived_bls_keypairs: HashMap<Pubkey, Arc<BLSKeypair>>,
    pub has_new_vote_been_rooted: bool,
    pub own_vote_sender: Sender<ConsensusMessage>,
    pub bls_sender: Sender<BLSOp>,
    pub commitment_sender: Sender<AlpenglowCommitmentAggregationData>,
    pub wait_to_vote_slot: Option<u64>,
    pub root_bank: SharableBank,
}

pub fn get_bls_keypair(
    context: &mut VotingContext,
    authorized_voter_keypair: &Arc<Keypair>,
) -> Result<Arc<BLSKeypair>, BlsError> {
    let pubkey = authorized_voter_keypair.pubkey();
    if let Some(existing) = context.derived_bls_keypairs.get(&pubkey) {
        return Ok(existing.clone());
    }

    let bls_keypair = Arc::new(BLSKeypair::derive_from_signer(
        authorized_voter_keypair,
        BLS_KEYPAIR_DERIVE_SEED,
    )?);

    context
        .derived_bls_keypairs
        .insert(pubkey, bls_keypair.clone());

    Ok(bls_keypair)
}

pub fn generate_vote_tx(
    vote: &Vote,
    bank: &Bank,
    context: &mut VotingContext,
) -> GenerateVoteTxResult {
    let vote_account_pubkey = context.vote_account_pubkey;
    let authorized_voter_keypair;
    let bls_pubkey_in_vote_account;
    {
        let authorized_voter_keypairs = context.authorized_voter_keypairs.read().unwrap();
        if !bank.has_initial_accounts_hash_verification_completed() {
            return GenerateVoteTxResult::WaitForStartupVerification;
        }
        if authorized_voter_keypairs.is_empty() {
            return GenerateVoteTxResult::NonVoting;
        }
        if let Some(slot) = context.wait_to_vote_slot {
            if vote.slot() < slot {
                return GenerateVoteTxResult::WaitToVoteSlot(slot);
            }
        }
        let Some(vote_account) = bank.get_vote_account(&vote_account_pubkey) else {
            return GenerateVoteTxResult::VoteAccountNotFound(vote_account_pubkey);
        };
        let Some(vote_state) = vote_account.alpenglow_vote_state() else {
            return GenerateVoteTxResult::NoVoteState(vote_account_pubkey);
        };
        if *vote_state.node_pubkey() != context.identity_keypair.pubkey() {
            info!(
                "Vote account node_pubkey mismatch: {} (expected: {}).  Unable to vote",
                vote_state.node_pubkey(),
                context.identity_keypair.pubkey()
            );
            return GenerateVoteTxResult::HotSpare;
        }
        bls_pubkey_in_vote_account = match vote_account.bls_pubkey() {
            None => {
                panic!(
                    "No BLS pubkey in vote account {}",
                    context.identity_keypair.pubkey()
                );
            }
            Some(key) => *key,
        };

        let Some(authorized_voter_pubkey) = vote_state.get_authorized_voter(bank.epoch()) else {
            return GenerateVoteTxResult::NoAuthorizedVoter(vote_account_pubkey, bank.epoch());
        };

        let Some(keypair) = authorized_voter_keypairs
            .iter()
            .find(|keypair| keypair.pubkey() == authorized_voter_pubkey)
        else {
            warn!(
                "The authorized keypair {authorized_voter_pubkey} for vote account \
                 {vote_account_pubkey} is not available.  Unable to vote"
            );
            return GenerateVoteTxResult::NonVoting;
        };

        authorized_voter_keypair = keypair.clone();
    }

    let bls_keypair = get_bls_keypair(context, &authorized_voter_keypair)
        .unwrap_or_else(|e| panic!("Failed to derive my own BLS keypair: {e:?}"));
    let my_bls_pubkey: BLSPubkey = bls_keypair.public;
    if my_bls_pubkey != bls_pubkey_in_vote_account {
        panic!(
            "Vote account bls_pubkey mismatch: {:?} (expected: {:?}).  Unable to vote",
            bls_pubkey_in_vote_account, my_bls_pubkey
        );
    }
    let vote_serialized = bincode::serialize(&vote).unwrap();

    let Some(epoch_stakes) = bank.epoch_stakes(bank.epoch()) else {
        panic!(
            "The bank {} doesn't have its own epoch_stakes for {}",
            bank.slot(),
            bank.epoch()
        );
    };
    let Some(my_rank) = epoch_stakes
        .bls_pubkey_to_rank_map()
        .get_rank(&my_bls_pubkey)
    else {
        return GenerateVoteTxResult::NoRankFound;
    };
    GenerateVoteTxResult::ConsensusMessage(ConsensusMessage::Vote(VoteMessage {
        vote: *vote,
        signature: bls_keypair.sign(&vote_serialized).into(),
        rank: *my_rank,
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

    let bank = context.root_bank.load();
    let message = match generate_vote_tx(&vote, &bank, context) {
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
        .send(message.clone())
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
            warn!("Failed to generate vote and push to votes: {:?}", e);
            // These are not fatal errors, just skip the vote for now. But they are misconfigurations
            // that should be warned about.
            return Ok(None);
        }
        Err(VoteError::TransientError(e)) => {
            info!("Failed to generate vote and push to votes: {:?}", e);
            // These are transient errors, just skip the vote for now.
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    Ok(Some(bls_op))
}
