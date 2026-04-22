//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.
use {
    crate::{
        packet::{PacketBatch, PacketFlags, PacketRefMut},
        recycled_vec::RecycledVec,
    },
    agave_transaction_view::{
        transaction_data::TransactionData, transaction_version::TransactionVersion,
        transaction_view::SanitizedTransactionView,
    },
    rayon::prelude::*,
};

// Empirically derived to constrain max verify latency to ~8ms at lower packet counts
pub const VERIFY_PACKET_CHUNK_SIZE: usize = 128;

pub type TxOffset = RecycledVec<u32>;

#[derive(Debug, PartialEq, Eq)]
pub enum PacketError {
    InvalidLen,
    InvalidPubkeyLen,
    InvalidShortVec,
    InvalidSignatureLen,
    MismatchSignatureLen,
    PayerNotWritable,
    InvalidProgramIdIndex,
    InvalidNumberOfInstructions,
    UnsupportedVersion,
}

impl std::convert::From<std::boxed::Box<bincode::ErrorKind>> for PacketError {
    fn from(_e: std::boxed::Box<bincode::ErrorKind>) -> PacketError {
        PacketError::InvalidShortVec
    }
}

impl std::convert::From<std::num::TryFromIntError> for PacketError {
    fn from(_e: std::num::TryFromIntError) -> Self {
        Self::InvalidLen
    }
}

/// Returns true if the signature on the packet verifies.
/// Caller must do packet.set_discard(true) if this returns false.
#[must_use]
fn verify_packet(packet: &mut PacketRefMut, reject_non_vote: bool) -> bool {
    // If this packet was already marked as discard, drop it
    if packet.meta().discard() {
        return false;
    }

    let Some(data) = packet.data(..) else {
        return false;
    };

    let (is_simple_vote_tx, verified) = {
        let Ok(view) = SanitizedTransactionView::try_new_sanitized(data, true) else {
            return false;
        };

        // Discard v1 transactions until support is added.
        if matches!(view.version(), TransactionVersion::V1) {
            return false;
        }

        let is_simple_vote_tx = is_simple_vote_transaction_view(&view);
        if reject_non_vote && !is_simple_vote_tx {
            (is_simple_vote_tx, false)
        } else {
            let signatures = view.signatures();
            if signatures.is_empty() {
                (is_simple_vote_tx, false)
            } else {
                let message = view.message_data();
                let static_account_keys = view.static_account_keys();
                let verified = signatures
                    .iter()
                    .zip(static_account_keys.iter())
                    .all(|(signature, pubkey)| signature.verify(pubkey.as_ref(), message));
                (is_simple_vote_tx, verified)
            }
        }
    };

    if is_simple_vote_tx {
        packet.meta_mut().flags |= PacketFlags::SIMPLE_VOTE_TX;
    }

    verified
}

pub fn count_packets_in_batches(batches: &[PacketBatch]) -> usize {
    batches.iter().map(|batch| batch.len()).sum()
}

pub fn count_valid_packets<'a>(batches: impl IntoIterator<Item = &'a PacketBatch>) -> usize {
    batches
        .into_iter()
        .map(|batch| batch.into_iter().filter(|p| !p.meta().discard()).count())
        .sum()
}

pub fn count_discarded_packets(batches: &[PacketBatch]) -> usize {
    batches
        .iter()
        .map(|batch| batch.iter().filter(|p| p.meta().discard()).count())
        .sum()
}

fn is_simple_vote_transaction_view<D: TransactionData>(view: &SanitizedTransactionView<D>) -> bool {
    // vote could have 1 or 2 sigs; zero sig has already been excluded by sanitization.
    if view.num_signatures() > 2 {
        return false;
    }

    // simple vote should only be legacy message
    if !matches!(view.version(), TransactionVersion::Legacy) {
        return false;
    }

    // skip if has more than 1 instruction
    if view.num_instructions() != 1 {
        return false;
    }

    let mut instructions = view.instructions_iter();
    let Some(instruction) = instructions.next() else {
        return false;
    };
    if instructions.next().is_some() {
        return false;
    }

    let program_id_index = usize::from(instruction.program_id_index);
    let Some(program_id) = view.static_account_keys().get(program_id_index) else {
        return false;
    };

    *program_id == solana_sdk_ids::vote::id()
}

pub fn ed25519_verify(
    thread_pool: &rayon::ThreadPool,
    batches: &mut [PacketBatch],
    reject_non_vote: bool,
    packet_count: usize,
) {
    debug!("CPU ECDSA for {packet_count}");
    thread_pool.install(|| {
        batches.par_iter_mut().flatten().for_each(|mut packet| {
            if !packet.meta().discard() && !verify_packet(&mut packet, reject_non_vote) {
                packet.meta_mut().set_discard(true);
            }
        });
    });
}

pub fn ed25519_verify_disabled(thread_pool: &rayon::ThreadPool, batches: &mut [PacketBatch]) {
    let packet_count = count_packets_in_batches(batches);
    debug!("disabled ECDSA for {packet_count}");

    thread_pool.install(|| {
        batches.par_iter_mut().flatten().for_each(|mut packet| {
            packet.meta_mut().set_discard(false);
        })
    });
}

pub fn mark_disabled(batches: &mut [PacketBatch], r: &[Vec<u8>]) {
    for (batch, v) in batches.iter_mut().zip(r) {
        for (mut pkt, f) in batch.iter_mut().zip(v) {
            if !pkt.meta().discard() {
                pkt.meta_mut().set_discard(*f == 0);
            }
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
pub fn threadpool_for_tests() -> rayon::ThreadPool {
    // Four threads is sufficient for unit tests
    rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .thread_name(|i| format!("solSigVerTest{i:02}"))
        .build()
        .expect("new rayon threadpool")
}

#[cfg(feature = "dev-context-only-utils")]
pub fn threadpool_for_benches() -> rayon::ThreadPool {
    let num_threads = (num_cpus::get() / 2).max(1);
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .thread_name(|i| format!("solSigVerBnch{i:02}"))
        .build()
        .expect("new rayon threadpool")
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use {
        super::*,
        crate::{
            packet::{BytesPacket, BytesPacketBatch},
            sigverify::{self},
            test_tx::{
                new_test_tx_with_number_of_ixs, new_test_vote_tx, test_multisig_tx, test_tx,
            },
        },
        bytes::Bytes,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{
            AccountMeta, Instruction, MESSAGE_VERSION_PREFIX, Message, MessageHeader,
            VersionedMessage, compiled_instruction::CompiledInstruction,
        },
        solana_pubkey::Pubkey,
        solana_signature::Signature,
        solana_signer::Signer,
        solana_transaction::{Transaction, versioned::VersionedTransaction},
        test_case::test_case,
    };

    fn new_test_vote_tx_v0() -> VersionedTransaction {
        let payer = Keypair::new();
        let instruction = Instruction {
            program_id: solana_vote_program::id(),
            accounts: vec![AccountMeta::new(payer.pubkey(), true)],
            data: vec![1, 2, 3],
        };
        let message = solana_message::v0::Message::try_compile(
            &payer.pubkey(),
            &[instruction],
            &[],
            Hash::new_unique(),
        )
        .unwrap();
        VersionedTransaction::try_new(VersionedMessage::V0(message), &[&payer]).unwrap()
    }

    #[test]
    fn test_mark_disabled() {
        let batch_size = 1;
        let mut batch = BytesPacketBatch::with_capacity(batch_size);
        batch.resize(batch_size, BytesPacket::empty());
        let mut batches: Vec<PacketBatch> = vec![batch.into()];
        mark_disabled(&mut batches, &[vec![0]]);
        assert!(batches[0].get(0).unwrap().meta().discard());
        batches[0].get_mut(0).unwrap().meta_mut().set_discard(false);
        mark_disabled(&mut batches, &[vec![1]]);
        assert!(!batches[0].get(0).unwrap().meta().discard());
    }

    fn packet_from_num_sigs(required_num_sigs: u8, actual_num_sigs: usize) -> BytesPacket {
        let message = Message {
            header: MessageHeader {
                num_required_signatures: required_num_sigs,
                num_readonly_signed_accounts: 12,
                num_readonly_unsigned_accounts: 11,
            },
            account_keys: vec![],
            recent_blockhash: Hash::default(),
            instructions: vec![],
        };
        let mut tx = Transaction::new_unsigned(message);
        tx.signatures = vec![Signature::default(); actual_num_sigs];
        BytesPacket::from_data(None, tx).unwrap()
    }

    #[test]
    fn test_untrustworthy_sigs() {
        let required_num_sigs = 14;
        let actual_num_sigs = 5;

        let mut packet = packet_from_num_sigs(required_num_sigs, actual_num_sigs);
        assert!(!sigverify::verify_packet(&mut packet.as_mut(), false));
    }

    #[test]
    fn test_small_packet() {
        let tx = test_tx();
        let mut data = bincode::serialize(&tx).unwrap();

        data[0] = 0xff;
        data[1] = 0xff;
        data.truncate(2);

        let mut packet = BytesPacket::from_bytes(None, Bytes::from(data));
        assert!(!sigverify::verify_packet(&mut packet.as_mut(), false));
    }

    #[test]
    fn test_pubkey_too_small() {
        agave_logger::setup();
        let mut tx = test_tx();
        let sig = tx.signatures[0];
        const NUM_SIG: usize = 18;
        tx.signatures = vec![sig; NUM_SIG];
        tx.message.account_keys = vec![];
        tx.message.header.num_required_signatures = NUM_SIG as u8;
        let mut packet = BytesPacket::from_data(None, tx).unwrap();

        assert!(!verify_packet(&mut packet.as_mut(), false));

        packet.meta_mut().set_discard(false);
        let mut batches = generate_packet_batches(&packet, 1, 1);
        ed25519_verify(&mut batches);
        assert!(batches[0].get(0).unwrap().meta().discard());
    }

    #[test]
    fn test_pubkey_len() {
        // See that the verify cannot walk off the end of the packet
        // trying to index into the account_keys to access pubkey.
        agave_logger::setup();

        const NUM_SIG: usize = 17;
        let keypair1 = Keypair::new();
        let pubkey1 = keypair1.pubkey();
        let mut message = Message::new(&[], Some(&pubkey1));
        message.account_keys.push(pubkey1);
        message.account_keys.push(pubkey1);
        message.header.num_required_signatures = NUM_SIG as u8;
        message.recent_blockhash = Hash::new_from_array(pubkey1.to_bytes());
        let mut tx = Transaction::new_unsigned(message);

        info!("message: {:?}", tx.message_data());
        info!("tx: {tx:?}");
        let sig = keypair1.try_sign_message(&tx.message_data()).unwrap();
        tx.signatures = vec![sig; NUM_SIG];

        let mut packet = BytesPacket::from_data(None, tx).unwrap();

        assert!(!verify_packet(&mut packet.as_mut(), false));

        packet.meta_mut().set_discard(false);
        let mut batches = generate_packet_batches(&packet, 1, 1);
        ed25519_verify(&mut batches);
        assert!(batches[0].get(0).unwrap().meta().discard());
    }

    #[test]
    fn test_large_sig_len() {
        let tx = test_tx();
        let mut data = bincode::serialize(&tx).unwrap();

        // Make the signatures len huge
        data[0] = 0x7f;

        let mut packet = BytesPacket::from_bytes(None, Bytes::from(data));
        assert!(!sigverify::verify_packet(&mut packet.as_mut(), false));
    }

    #[test]
    fn test_really_large_sig_len() {
        let tx = test_tx();
        let mut data = bincode::serialize(&tx).unwrap();

        // Make the signatures len huge
        data[0] = 0xff;
        data[1] = 0xff;
        data[2] = 0xff;
        data[3] = 0xff;

        let mut packet = BytesPacket::from_bytes(None, Bytes::from(data));
        assert!(!sigverify::verify_packet(&mut packet.as_mut(), false));
    }

    #[test]
    fn test_invalid_pubkey_len() {
        let tx = test_tx();
        let mut data = bincode::serialize(&tx).unwrap();

        // make pubkey len huge
        const PUBKEY_OFFSET: usize =
            1 + core::mem::size_of::<Signature>() + core::mem::size_of::<MessageHeader>();
        data[PUBKEY_OFFSET] = 0x7f;

        let mut packet = BytesPacket::from_bytes(None, Bytes::from(data));
        assert!(!sigverify::verify_packet(&mut packet.as_mut(), false));
    }

    #[test]
    fn test_fee_payer_is_debitable() {
        let message = Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 1,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![],
            recent_blockhash: Hash::default(),
            instructions: vec![],
        };
        let mut tx = Transaction::new_unsigned(message);
        tx.signatures = vec![Signature::default()];
        let mut packet = BytesPacket::from_data(None, tx).unwrap();
        assert!(!sigverify::verify_packet(&mut packet.as_mut(), false));
    }

    #[test]
    fn test_unsupported_version() {
        let tx = test_tx();
        let mut data = bincode::serialize(&tx).unwrap();

        // set message version to 1
        const MESSAGE_OFFSET: usize = 1 + core::mem::size_of::<Signature>();
        data[MESSAGE_OFFSET] = MESSAGE_VERSION_PREFIX + 1;

        let mut packet = BytesPacket::from_bytes(None, Bytes::from(data));
        assert!(!sigverify::verify_packet(&mut packet.as_mut(), false));
    }

    fn generate_bytes_packet_batches(
        packet: &BytesPacket,
        num_packets_per_batch: usize,
        num_batches: usize,
    ) -> Vec<BytesPacketBatch> {
        let batches: Vec<BytesPacketBatch> = (0..num_batches)
            .map(|_| {
                let mut packet_batch = BytesPacketBatch::with_capacity(num_packets_per_batch);
                for _ in 0..num_packets_per_batch {
                    packet_batch.push(packet.clone());
                }
                assert_eq!(packet_batch.len(), num_packets_per_batch);
                packet_batch
            })
            .collect();
        assert_eq!(batches.len(), num_batches);

        batches
    }

    fn generate_packet_batches(
        packet: &BytesPacket,
        num_packets_per_batch: usize,
        num_batches: usize,
    ) -> Vec<PacketBatch> {
        // generate packet vector
        let batches: Vec<PacketBatch> = (0..num_batches)
            .map(|_| {
                let mut packet_batch = BytesPacketBatch::with_capacity(num_packets_per_batch);
                for _ in 0..num_packets_per_batch {
                    packet_batch.push(packet.clone());
                }
                assert_eq!(packet_batch.len(), num_packets_per_batch);
                packet_batch.into()
            })
            .collect();
        assert_eq!(batches.len(), num_batches);

        batches
    }

    fn test_verify_n(n: usize, modify_data: bool) {
        let tx = test_tx();
        let mut data = bincode::serialize(&tx).unwrap();

        // jumble some data to test failure
        if modify_data {
            data[20] = data[20].wrapping_add(10);
        }

        let packet = BytesPacket::from_bytes(None, Bytes::from(data));
        let mut batches = generate_packet_batches(&packet, n, 2);

        // verify packets
        ed25519_verify(&mut batches);

        // check result
        let should_discard = modify_data;
        assert!(
            batches
                .iter()
                .flat_map(|batch| batch.iter())
                .all(|p| p.meta().discard() == should_discard)
        );
    }

    fn ed25519_verify(batches: &mut [PacketBatch]) {
        let threadpool = threadpool_for_tests();
        let packet_count = sigverify::count_packets_in_batches(batches);
        sigverify::ed25519_verify(&threadpool, batches, false, packet_count);
    }

    #[test]
    fn test_verify_tampered_sig_len() {
        let mut tx = test_tx();
        // pretend malicious leader dropped a signature...
        tx.signatures.pop();
        let packet = BytesPacket::from_data(None, tx).unwrap();

        let mut batches = generate_packet_batches(&packet, 1, 1);

        // verify packets
        ed25519_verify(&mut batches);
        assert!(
            batches
                .iter()
                .flat_map(|batch| batch.iter())
                .all(|p| p.meta().discard())
        );
    }

    #[test]
    fn test_verify_zero() {
        test_verify_n(0, false);
    }

    #[test]
    fn test_verify_one() {
        test_verify_n(1, false);
    }

    #[test]
    fn test_verify_seventy_one() {
        test_verify_n(71, false);
    }

    #[test]
    fn test_verify_medium_pass() {
        test_verify_n(VERIFY_PACKET_CHUNK_SIZE, false);
    }

    #[test]
    fn test_verify_large_pass() {
        test_verify_n(VERIFY_PACKET_CHUNK_SIZE * 32, false);
    }

    #[test]
    fn test_verify_medium_fail() {
        test_verify_n(VERIFY_PACKET_CHUNK_SIZE, true);
    }

    #[test]
    fn test_verify_large_fail() {
        test_verify_n(VERIFY_PACKET_CHUNK_SIZE * 32, true);
    }

    #[test]
    fn test_verify_multisig() {
        agave_logger::setup();

        let tx = test_multisig_tx();
        let mut data = bincode::serialize(&tx).unwrap();

        let n = 4;
        let num_batches = 3;
        let packet = BytesPacket::from_bytes(None, Bytes::from(data.clone()));
        let mut batches = generate_bytes_packet_batches(&packet, n, num_batches);

        data[40] = data[40].wrapping_add(8);
        let packet = BytesPacket::from_bytes(None, Bytes::from(data.clone()));

        batches[0].push(packet);

        // verify packets
        let mut batches: Vec<PacketBatch> = batches.into_iter().map(PacketBatch::from).collect();
        ed25519_verify(&mut batches);

        // check result
        let ref_ans = 1u8;
        let mut ref_vec = vec![vec![ref_ans; n]; num_batches];
        ref_vec[0].push(0u8);
        assert!(
            batches
                .iter()
                .flat_map(|batch| batch.iter())
                .zip(ref_vec.into_iter().flatten())
                .all(|(p, discard)| {
                    if discard == 0 {
                        p.meta().discard()
                    } else {
                        !p.meta().discard()
                    }
                })
        );
    }

    #[test]
    fn test_verify_fail() {
        test_verify_n(5, true);
    }

    #[test]
    fn test_is_simple_vote_transaction() {
        agave_logger::setup();
        let mut rng = rand::rng();

        // transfer tx is not
        {
            let mut tx = test_tx();
            tx.message.instructions[0].data = vec![1, 2, 3];
            let packet = BytesPacket::from_data(None, tx).unwrap();
            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                true,
            )
            .unwrap();
            assert!(!is_simple_vote_transaction_view(&view));
        }

        // single legacy vote tx is
        {
            let mut tx = new_test_vote_tx(&mut rng);
            tx.message.instructions[0].data = vec![1, 2, 3];
            let packet = BytesPacket::from_data(None, tx).unwrap();
            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                true,
            )
            .unwrap();
            assert!(is_simple_vote_transaction_view(&view));
        }

        // single versioned vote tx is not
        {
            let tx = new_test_vote_tx_v0();
            let packet = BytesPacket::from_data(None, tx).unwrap();

            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                true,
            )
            .unwrap();
            assert!(!is_simple_vote_transaction_view(&view));
            assert!(!packet.meta().is_simple_vote_tx());
        }

        // multiple mixed tx is not
        {
            let key = Keypair::new();
            let key1 = Pubkey::new_unique();
            let key2 = Pubkey::new_unique();
            let tx = Transaction::new_with_compiled_instructions(
                &[&key],
                &[key1, key2],
                Hash::default(),
                vec![solana_vote_program::id(), Pubkey::new_unique()],
                vec![
                    CompiledInstruction::new(3, &(), vec![0, 1]),
                    CompiledInstruction::new(4, &(), vec![0, 2]),
                ],
            );
            let packet = BytesPacket::from_data(None, tx).unwrap();
            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                true,
            )
            .unwrap();
            assert!(!is_simple_vote_transaction_view(&view));
        }

        // single legacy vote tx with extra (invalid) signature is not
        {
            let mut tx = new_test_vote_tx(&mut rng);
            tx.signatures.push(Signature::default());
            tx.message.header.num_required_signatures = 3;
            tx.message.instructions[0].data = vec![1, 2, 3];
            let packet = BytesPacket::from_data(None, tx).unwrap();
            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                true,
            )
            .unwrap();
            assert!(!is_simple_vote_transaction_view(&view));
        }
    }

    #[test_case(false, false; "ok_ixs_legacy")]
    #[test_case(true, false; "too_many_ixs_legacy")]
    #[test_case(false, true; "ok_ixs_versioned")]
    #[test_case(true, true; "too_many_ixs_versioned")]
    fn test_number_of_instructions(too_many_ixs: bool, is_versioned_tx: bool) {
        let mut number_of_ixs = 64;
        if too_many_ixs {
            number_of_ixs += 1;
        }

        let mut packet = if is_versioned_tx {
            let tx: VersionedTransaction = new_test_tx_with_number_of_ixs(number_of_ixs);
            BytesPacket::from_data(None, tx.clone()).unwrap()
        } else {
            let tx: Transaction = new_test_tx_with_number_of_ixs(number_of_ixs);
            BytesPacket::from_data(None, tx.clone()).unwrap()
        };

        assert_eq!(
            sigverify::verify_packet(&mut packet.as_mut(), false),
            !too_many_ixs
        );
    }
}
