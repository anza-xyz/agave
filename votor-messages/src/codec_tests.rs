//! Property and adversarial-input tests for the Alpenglow consensus wire codec.
//!
//! Consensus messages arrive from untrusted peers, so the `wincode` (de)serializer
//! for these types must uphold three invariants:
//!
//! 1. **Round-trip stability** — every well-formed value survives
//!    `serialize` -> `deserialize` unchanged.
//! 2. **Canonical encoding** — re-serializing a decoded value yields identical bytes.
//! 3. **Hostile-input safety** — `deserialize` never panics on arbitrary or
//!    corrupted bytes, and any value it accepts re-serializes stably.

use {
    crate::{
        certificate::{Certificate, CertificateType},
        consensus_message::{Block, ConsensusMessage, VoteMessage},
        vote::Vote,
    },
    proptest::prelude::*,
    solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BlsSignature},
    solana_hash::Hash,
};

fn arb_hash() -> impl Strategy<Value = Hash> {
    prop::collection::vec(any::<u8>(), 32)
        .prop_map(|bytes| Hash::new_from_array(bytes.try_into().unwrap()))
}

fn arb_block() -> impl Strategy<Value = Block> {
    (any::<u64>(), arb_hash()).prop_map(|(slot, block_id)| Block { slot, block_id })
}

fn arb_signature() -> impl Strategy<Value = BlsSignature> {
    // Bytes need not form a valid curve point: the wire codec stores the raw
    // affine bytes, so round-trip behavior is independent of point validity.
    prop::collection::vec(any::<u8>(), BLS_SIGNATURE_AFFINE_SIZE).prop_map(|bytes| {
        let mut arr = [0u8; BLS_SIGNATURE_AFFINE_SIZE];
        arr.copy_from_slice(&bytes);
        BlsSignature(arr)
    })
}

fn arb_vote() -> impl Strategy<Value = Vote> {
    prop_oneof![
        arb_block().prop_map(Vote::new_notarization_vote),
        any::<u64>().prop_map(Vote::new_finalization_vote),
        any::<u64>().prop_map(Vote::new_skip_vote),
        arb_block().prop_map(Vote::new_notarization_fallback_vote),
        any::<u64>().prop_map(Vote::new_skip_fallback_vote),
        arb_block().prop_map(Vote::new_genesis_vote),
    ]
}

fn arb_cert_type() -> impl Strategy<Value = CertificateType> {
    prop_oneof![
        any::<u64>().prop_map(CertificateType::Finalize),
        arb_block().prop_map(CertificateType::FinalizeFast),
        arb_block().prop_map(CertificateType::Notarize),
        arb_block().prop_map(CertificateType::NotarizeFallback),
        any::<u64>().prop_map(CertificateType::Skip),
        arb_block().prop_map(CertificateType::Genesis),
    ]
}

fn arb_vote_message() -> impl Strategy<Value = VoteMessage> {
    (arb_vote(), arb_signature(), any::<u16>()).prop_map(|(vote, signature, rank)| VoteMessage {
        vote,
        signature,
        rank,
    })
}

fn arb_certificate() -> impl Strategy<Value = Certificate> {
    (
        arb_cert_type(),
        arb_signature(),
        prop::collection::vec(any::<u8>(), 0..64),
    )
        .prop_map(|(cert_type, signature, bitmap)| Certificate {
            cert_type,
            signature,
            bitmap,
        })
}

fn arb_consensus_message() -> impl Strategy<Value = ConsensusMessage> {
    prop_oneof![
        arb_vote_message().prop_map(ConsensusMessage::Vote),
        arb_certificate().prop_map(ConsensusMessage::Certificate),
    ]
}

/// Decode `bytes` as a `ConsensusMessage`; if accepted, assert it re-encodes
/// byte-stably. Returns whether the bytes were accepted, so callers can confirm the
/// accept path is actually being exercised.
fn accepted_and_stable(bytes: &[u8]) -> bool {
    match wincode::deserialize::<ConsensusMessage>(bytes) {
        Ok(message) => {
            let reserialized =
                wincode::serialize(&message).expect("accepted message must re-serialize");
            let redecoded: ConsensusMessage =
                wincode::deserialize(&reserialized).expect("re-serialized message must decode");
            assert_eq!(message, redecoded);
            true
        }
        Err(_) => false,
    }
}

/// A few concrete, valid consensus messages used to seed mutation testing.
fn sample_messages() -> Vec<ConsensusMessage> {
    let signature = BlsSignature([7u8; BLS_SIGNATURE_AFFINE_SIZE]);
    let block = Block {
        slot: 42,
        block_id: Hash::new_from_array([3u8; 32]),
    };
    vec![
        ConsensusMessage::Vote(VoteMessage {
            vote: Vote::new_notarization_vote(block),
            signature,
            rank: 5,
        }),
        ConsensusMessage::Vote(VoteMessage {
            vote: Vote::new_finalization_vote(42),
            signature,
            rank: 0,
        }),
        ConsensusMessage::Certificate(Certificate {
            cert_type: CertificateType::Notarize(block),
            signature,
            bitmap: vec![0, 3, 0],
        }),
        ConsensusMessage::Certificate(Certificate {
            cert_type: CertificateType::Skip(42),
            signature,
            bitmap: vec![1, 2, 3, 4],
        }),
    ]
}

proptest! {
    #[test]
    fn vote_round_trips(value in arb_vote()) {
        let bytes = wincode::serialize(&value).expect("serialize Vote");
        let decoded: Vote = wincode::deserialize(&bytes).expect("deserialize Vote");
        prop_assert_eq!(value, decoded);
    }

    #[test]
    fn cert_type_round_trips(value in arb_cert_type()) {
        let bytes = wincode::serialize(&value).expect("serialize CertificateType");
        let decoded: CertificateType =
            wincode::deserialize(&bytes).expect("deserialize CertificateType");
        prop_assert_eq!(value, decoded);
    }

    #[test]
    fn vote_message_round_trips(value in arb_vote_message()) {
        let bytes = wincode::serialize(&value).expect("serialize VoteMessage");
        let decoded: VoteMessage = wincode::deserialize(&bytes).expect("deserialize VoteMessage");
        prop_assert_eq!(value, decoded);
    }

    #[test]
    fn certificate_round_trips(value in arb_certificate()) {
        let bytes = wincode::serialize(&value).expect("serialize Certificate");
        let decoded: Certificate = wincode::deserialize(&bytes).expect("deserialize Certificate");
        prop_assert_eq!(value, decoded);
    }

    #[test]
    fn consensus_message_round_trips(value in arb_consensus_message()) {
        let bytes = wincode::serialize(&value).expect("serialize ConsensusMessage");
        let decoded: ConsensusMessage =
            wincode::deserialize(&bytes).expect("deserialize ConsensusMessage");
        prop_assert_eq!(value, decoded);
    }

    /// Encoding is deterministic: re-serializing a decoded value reproduces the
    /// exact same bytes (no non-canonical or unstable representation).
    #[test]
    fn encoding_is_canonical(value in arb_consensus_message()) {
        let bytes = wincode::serialize(&value).expect("serialize");
        let decoded: ConsensusMessage = wincode::deserialize(&bytes).expect("deserialize");
        let reencoded = wincode::serialize(&decoded).expect("re-serialize");
        prop_assert_eq!(bytes, reencoded);
    }

    /// Deserializing arbitrary attacker-controlled bytes must never panic for any of
    /// the wire types. (Random bytes essentially never form a valid message — the
    /// accept path is exercised by `single_byte_mutation_is_safe_and_stable`.)
    #[test]
    fn deserialize_of_arbitrary_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), 0..256)
    ) {
        let _ = wincode::deserialize::<Vote>(&data);
        let _ = wincode::deserialize::<CertificateType>(&data);
        let _ = wincode::deserialize::<VoteMessage>(&data);
        let _ = wincode::deserialize::<Certificate>(&data);
        let _ = wincode::deserialize::<ConsensusMessage>(&data);
    }
}

/// Corrupting a single byte of a valid message must never panic the decoder, and any
/// corrupted message that still decodes must re-encode stably. Unlike fully random
/// bytes (which never decode), single-byte mutations frequently remain valid, so this
/// genuinely exercises the accept path — the final assertion guards against the test
/// silently becoming vacuous.
#[test]
fn single_byte_mutation_is_safe_and_stable() {
    let mut accepted = 0u64;
    for message in sample_messages() {
        let bytes = wincode::serialize(&message).expect("serialize sample");
        for index in 0..bytes.len() {
            for delta in [1u8, 0x55, 0x80, 0xff] {
                let mut corrupted = bytes.clone();
                corrupted[index] = corrupted[index].wrapping_add(delta);
                if accepted_and_stable(&corrupted) {
                    accepted += 1;
                }
            }
        }
    }
    assert!(
        accepted > 0,
        "no mutated message was accepted; the stability path was never exercised"
    );
}
