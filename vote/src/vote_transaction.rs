use {
    serde_derive::{Deserialize, Serialize},
    solana_clock::{Slot, UnixTimestamp},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_transaction::Transaction,
    solana_vote_interface::{self as vote, state::*},
};

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor),
    frozen_abi(digest = "5tNdc77vxH68VdiV2CNwXRhcJzDMk2ncYXYw3JQdMYhd")
)]
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum VoteTransaction {
    Vote(Vote),
    VoteStateUpdate(VoteStateUpdate),
    #[serde(with = "serde_compact_vote_state_update")]
    CompactVoteStateUpdate(VoteStateUpdate),
    #[serde(with = "serde_tower_sync")]
    TowerSync(TowerSync),
}

impl VoteTransaction {
    pub fn slots(&self) -> Vec<Slot> {
        match self {
            VoteTransaction::Vote(vote) => vote.slots.clone(),
            VoteTransaction::VoteStateUpdate(vote_state_update) => vote_state_update.slots(),
            VoteTransaction::CompactVoteStateUpdate(vote_state_update) => vote_state_update.slots(),
            VoteTransaction::TowerSync(tower_sync) => tower_sync.slots(),
        }
    }

    pub fn slot(&self, i: usize) -> Slot {
        match self {
            VoteTransaction::Vote(vote) => vote.slots[i],
            VoteTransaction::VoteStateUpdate(vote_state_update)
            | VoteTransaction::CompactVoteStateUpdate(vote_state_update) => {
                vote_state_update.lockouts[i].slot()
            }
            VoteTransaction::TowerSync(tower_sync) => tower_sync.lockouts[i].slot(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            VoteTransaction::Vote(vote) => vote.slots.len(),
            VoteTransaction::VoteStateUpdate(vote_state_update)
            | VoteTransaction::CompactVoteStateUpdate(vote_state_update) => {
                vote_state_update.lockouts.len()
            }
            VoteTransaction::TowerSync(tower_sync) => tower_sync.lockouts.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            VoteTransaction::Vote(vote) => vote.slots.is_empty(),
            VoteTransaction::VoteStateUpdate(vote_state_update)
            | VoteTransaction::CompactVoteStateUpdate(vote_state_update) => {
                vote_state_update.lockouts.is_empty()
            }
            VoteTransaction::TowerSync(tower_sync) => tower_sync.lockouts.is_empty(),
        }
    }

    pub fn hash(&self) -> Hash {
        match self {
            VoteTransaction::Vote(vote) => vote.hash,
            VoteTransaction::VoteStateUpdate(vote_state_update) => vote_state_update.hash,
            VoteTransaction::CompactVoteStateUpdate(vote_state_update) => vote_state_update.hash,
            VoteTransaction::TowerSync(tower_sync) => tower_sync.hash,
        }
    }

    pub fn timestamp(&self) -> Option<UnixTimestamp> {
        match self {
            VoteTransaction::Vote(vote) => vote.timestamp,
            VoteTransaction::VoteStateUpdate(vote_state_update)
            | VoteTransaction::CompactVoteStateUpdate(vote_state_update) => {
                vote_state_update.timestamp
            }
            VoteTransaction::TowerSync(tower_sync) => tower_sync.timestamp,
        }
    }

    pub fn set_timestamp(&mut self, ts: Option<UnixTimestamp>) {
        match self {
            VoteTransaction::Vote(vote) => vote.timestamp = ts,
            VoteTransaction::VoteStateUpdate(vote_state_update)
            | VoteTransaction::CompactVoteStateUpdate(vote_state_update) => {
                vote_state_update.timestamp = ts
            }
            VoteTransaction::TowerSync(tower_sync) => tower_sync.timestamp = ts,
        }
    }

    pub fn last_voted_slot(&self) -> Option<Slot> {
        match self {
            VoteTransaction::Vote(vote) => vote.last_voted_slot(),
            VoteTransaction::VoteStateUpdate(vote_state_update)
            | VoteTransaction::CompactVoteStateUpdate(vote_state_update) => {
                vote_state_update.last_voted_slot()
            }
            VoteTransaction::TowerSync(tower_sync) => tower_sync.last_voted_slot(),
        }
    }

    pub fn last_voted_slot_hash(&self) -> Option<(Slot, Hash)> {
        Some((self.last_voted_slot()?, self.hash()))
    }

    pub fn is_full_tower_vote(&self) -> bool {
        matches!(
            self,
            VoteTransaction::VoteStateUpdate(_) | VoteTransaction::TowerSync(_)
        )
    }
}

impl From<Vote> for VoteTransaction {
    fn from(vote: Vote) -> Self {
        VoteTransaction::Vote(vote)
    }
}

impl From<VoteStateUpdate> for VoteTransaction {
    fn from(vote_state_update: VoteStateUpdate) -> Self {
        VoteTransaction::VoteStateUpdate(vote_state_update)
    }
}

impl From<TowerSync> for VoteTransaction {
    fn from(tower_sync: TowerSync) -> Self {
        VoteTransaction::TowerSync(tower_sync)
    }
}

pub fn new_vote_transaction(
    slots: Vec<Slot>,
    bank_hash: Hash,
    blockhash: Hash,
    node_keypair: &Keypair,
    vote_keypair: &Keypair,
    authorized_voter_keypair: &Keypair,
    switch_proof_hash: Option<Hash>,
) -> Transaction {
    let votes = Vote::new(slots, bank_hash);
    let vote_ix = if let Some(switch_proof_hash) = switch_proof_hash {
        vote::instruction::vote_switch(
            &vote_keypair.pubkey(),
            &authorized_voter_keypair.pubkey(),
            votes,
            switch_proof_hash,
        )
    } else {
        vote::instruction::vote(
            &vote_keypair.pubkey(),
            &authorized_voter_keypair.pubkey(),
            votes,
        )
    };

    let mut vote_tx = Transaction::new_with_payer(&[vote_ix], Some(&node_keypair.pubkey()));

    vote_tx.partial_sign(&[node_keypair], blockhash);
    vote_tx.partial_sign(&[authorized_voter_keypair], blockhash);
    vote_tx
}

pub fn new_vote_state_update_transaction(
    vote_state_update: VoteStateUpdate,
    blockhash: Hash,
    node_keypair: &Keypair,
    vote_keypair: &Keypair,
    authorized_voter_keypair: &Keypair,
    switch_proof_hash: Option<Hash>,
) -> Transaction {
    let vote_ix = if let Some(switch_proof_hash) = switch_proof_hash {
        vote::instruction::update_vote_state_switch(
            &vote_keypair.pubkey(),
            &authorized_voter_keypair.pubkey(),
            vote_state_update,
            switch_proof_hash,
        )
    } else {
        vote::instruction::update_vote_state(
            &vote_keypair.pubkey(),
            &authorized_voter_keypair.pubkey(),
            vote_state_update,
        )
    };

    let mut vote_tx = Transaction::new_with_payer(&[vote_ix], Some(&node_keypair.pubkey()));

    vote_tx.partial_sign(&[node_keypair], blockhash);
    vote_tx.partial_sign(&[authorized_voter_keypair], blockhash);
    vote_tx
}

pub fn new_compact_vote_state_update_transaction(
    vote_state_update: VoteStateUpdate,
    blockhash: Hash,
    node_keypair: &Keypair,
    vote_keypair: &Keypair,
    authorized_voter_keypair: &Keypair,
    switch_proof_hash: Option<Hash>,
) -> Transaction {
    let vote_ix = if let Some(switch_proof_hash) = switch_proof_hash {
        vote::instruction::compact_update_vote_state_switch(
            &vote_keypair.pubkey(),
            &authorized_voter_keypair.pubkey(),
            vote_state_update,
            switch_proof_hash,
        )
    } else {
        vote::instruction::compact_update_vote_state(
            &vote_keypair.pubkey(),
            &authorized_voter_keypair.pubkey(),
            vote_state_update,
        )
    };

    let mut vote_tx = Transaction::new_with_payer(&[vote_ix], Some(&node_keypair.pubkey()));

    vote_tx.partial_sign(&[node_keypair], blockhash);
    vote_tx.partial_sign(&[authorized_voter_keypair], blockhash);
    vote_tx
}

#[must_use]
pub fn new_tower_sync_transaction(
    tower_sync: TowerSync,
    blockhash: Hash,
    node_keypair: &Keypair,
    vote_keypair: &Keypair,
    authorized_voter_keypair: &Keypair,
    switch_proof_hash: Option<Hash>,
) -> Transaction {
    let vote_ix = if let Some(switch_proof_hash) = switch_proof_hash {
        vote::instruction::tower_sync_switch(
            &vote_keypair.pubkey(),
            &authorized_voter_keypair.pubkey(),
            tower_sync,
            switch_proof_hash,
        )
    } else {
        vote::instruction::tower_sync(
            &vote_keypair.pubkey(),
            &authorized_voter_keypair.pubkey(),
            tower_sync,
        )
    };

    let mut vote_tx = Transaction::new_with_payer(&[vote_ix], Some(&node_keypair.pubkey()));

    vote_tx.partial_sign(&[node_keypair], blockhash);
    vote_tx.partial_sign(&[authorized_voter_keypair], blockhash);
    vote_tx
}
