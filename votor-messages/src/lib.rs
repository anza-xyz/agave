#![cfg(feature = "agave-unstable-api")]
//! Alpenglow vote message types
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(missing_docs)]

use crate::consensus_message::Block;

use {
    crossbeam_channel::{Receiver, Sender},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
};

pub mod certificate;
pub mod consensus_message;
pub mod finalized_slot;
pub mod fraction;
pub mod metric_types;
pub mod migration;
pub mod reward_certificate;
pub mod unverified_vote_message;
pub mod vote;
pub mod wire;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

/// Send side of verified voter channel.
/// Each message contains the Pubkey of the voter and the slots in last verified vote.
pub type VerifiedVoterSlotsSender = Sender<(Pubkey, Vec<Slot>)>;
/// Receive side of verified voter channel.
pub type VerifiedVoterSlotsReceiver = Receiver<(Pubkey, Vec<Slot>)>;

// TODO: should the sender have to sign the rank on the votes as well?  Currently it is not signed.

use solana_bls_signatures::Signature as BLSSignature;

// ============ votes ====================

/// Internal representation of a vote.  It doesn't have frozen API.
enum Vote {
    Notar(Block),
}

/// This is part of the payload that will be signed by the sender.  It has the enum tags.  This should have frozen api on it.
enum WireVoteKind {
    Notar(Block),
}

/// This is the complete payload that will be signed by the sender.  This should have frozen api on it.
struct WireVote {
    shred_version: u16,
    kind: WireVoteKind,
}

/// The complete content of a wire representation of a vote.  This needs frozen api on it.  The function that produces this struct also produces VerifiedVoteMessage to send on the own message channel.
struct WireVoteMessage {
    payload: WireVote,
    rank: u16,
    signature: BLSSignature,
}

/// Computed from VersionedWireConsensusMessage at the receiver.  This doesn't need frozen api on it.
struct UnverifiedWireVote {
    vote: Vote,
    rank: u16,
    signature: BLSSignature,
}

/// Upon verifying UnverifiedWireVote, the receiver will produce the following struct.  This doesn't need frozen api on it.  Also as noted above, this will also be produced when the sender is producing WireVoteMessage.
struct VerifiedVoteMessage {
    vote: Vote,
    rank: u16,
    signature: BLSSignature,
}

/// When we work on the bls-malleability issue, we will compute this instead of VerifiedVoteMessage.  Upon verifying a batch of UnverifiedWireVote, the receiver will produce the following struct.  This doesn't need frozen api on it.
struct VerifiedVoteBatch {
    vote: Vote,
    rank: u16,
    signature: BLSSignature,
}

// ============ certificates ====================

/// Internal representation of a certificate.  Doesn't need frozen api on it.
enum Certificate {
    Notar(Block),
}

/// Internal representation of a certificate message.  This doesn't needs frozen api on it.  Built by combining one or more ReceiverVote (or ReceiverVoteBatch).  Alternatively also built by verifying UnverifiedCertificateMessage.
struct CertificateMessage {
    cert: Certificate,
    signature: BLSSignature,
    bitmap: Vec<u8>,
}

/// Internal representation of an unverified cert message.  Computed from VersionedWireConsensusMessage by the receiver.  Doesn't need frozen api.
struct UnverifiedCertificateMessage {
    cert: Certificate,
    signature: BLSSignature,
    bitmap: Vec<u8>,
}

/// Wire representation of a certificate.  This needs frozen api on it.  Although looks similar to Certificate, defining it separately allows us to modify the internal representation without requiring a SIMD.
enum WireCertificate {
    Notar(Block),
}

/// Wire representation of a certificate message.  This needs frozen api on it.  Although looks similar to CertificateMessage, defining it separately allows us to modify the internal representation without requiring a SIMD.
struct WireCertificateMessage {
    cert: WireCertificate,
    signature: BLSSignature,
    bitmap: Vec<u8>,
}

// ============ wire consensus messages ====================

/// Part of what is transmitted over the wire.  This needs frozen api.
enum WireConsensusMessageKind {
    NotarVote(WireVoteMessage),
    NotarCert(WireCertificateMessage),
}

/// Add shred_version to kind to do early filtering.  This needs frozen api.
struct WireConsensusMessageV1 {
    kind: WireConsensusMessageKind,
    shred_version: u16,
}

/// Versioned wire consensus message.  This needs frozen api on it.
enum VersionedWireConsensusMessage {
    V1(WireConsensusMessageV1),
}
