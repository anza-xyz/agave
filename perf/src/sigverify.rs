//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.
use {
    crate::{
        packet::{BytesPacketBatch, PacketBatch, PacketFlags, PacketRefMut, RecycledPacketBatch},
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
        let Ok(view) = SanitizedTransactionView::try_new_sanitized(data, true, true) else {
            return false;
        };

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

fn split_batches(batches: Vec<PacketBatch>) -> (Vec<BytesPacketBatch>, Vec<RecycledPacketBatch>) {
    let mut bytes_batches = Vec::new();
    let mut pinned_batches = Vec::new();
    for batch in batches {
        match batch {
            PacketBatch::Bytes(batch) => bytes_batches.push(batch),
            PacketBatch::Pinned(batch) => pinned_batches.push(batch),
            PacketBatch::Single(packet) => {
                let mut batch = BytesPacketBatch::with_capacity(1);
                batch.push(packet);
                bytes_batches.push(batch);
            }
        }
    }
    (bytes_batches, pinned_batches)
}

macro_rules! shrink_batches_fn {
    ($fn_name:ident, $batch_ty:ty) => {
        fn $fn_name(batches: &mut Vec<$batch_ty>) {
            let mut valid_batch_ix = 0;
            let mut valid_packet_ix = 0;
            let mut last_valid_batch = 0;
            for batch_ix in 0..batches.len() {
                let cur_batch = batches.get_mut(batch_ix).unwrap();
                for packet_ix in 0..cur_batch.len() {
                    if batches[batch_ix][packet_ix].meta().discard() {
                        continue;
                    }
                    last_valid_batch = batch_ix.saturating_add(1);
                    let mut found_spot = false;
                    while valid_batch_ix < batch_ix && !found_spot {
                        while valid_packet_ix < batches[valid_batch_ix].len() {
                            if batches[valid_batch_ix][valid_packet_ix].meta().discard() {
                                batches[valid_batch_ix][valid_packet_ix] =
                                    batches[batch_ix][packet_ix].clone();
                                batches[batch_ix][packet_ix].meta_mut().set_discard(true);
                                last_valid_batch = valid_batch_ix.saturating_add(1);
                                found_spot = true;
                                break;
                            }
                            valid_packet_ix = valid_packet_ix.saturating_add(1);
                        }
                        if valid_packet_ix >= batches[valid_batch_ix].len() {
                            valid_packet_ix = 0;
                            valid_batch_ix = valid_batch_ix.saturating_add(1);
                        }
                    }
                }
            }
            batches.truncate(last_valid_batch);
        }
    };
}

shrink_batches_fn!(shrink_bytes_batches, BytesPacketBatch);
shrink_batches_fn!(shrink_pinned_batches, RecycledPacketBatch);

pub fn shrink_batches(batches: Vec<PacketBatch>) -> Vec<PacketBatch> {
    let (mut bytes_batches, mut pinned_batches) = split_batches(batches);
    shrink_bytes_batches(&mut bytes_batches);
    shrink_pinned_batches(&mut pinned_batches);
    bytes_batches
        .into_iter()
        .map(PacketBatch::Bytes)
        .chain(pinned_batches.into_iter().map(PacketBatch::Pinned))
        .collect()
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
            packet::{
                to_packet_batches, BytesPacket, BytesPacketBatch, Packet, RecycledPacketBatch,
                PACKETS_PER_BATCH,
            },
            sigverify::{self},
            test_tx::{
                new_test_tx_with_number_of_ixs, new_test_vote_tx, test_multisig_tx, test_tx,
            },
        },
        bytes::Bytes,
        rand::Rng,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{
            compiled_instruction::CompiledInstruction, AccountMeta, Instruction, Message,
            MessageHeader, VersionedMessage, MESSAGE_VERSION_PREFIX,
        },
        solana_pubkey::Pubkey,
        solana_signature::Signature,
        solana_signer::Signer,
        solana_transaction::{versioned::VersionedTransaction, Transaction},
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
        assert!(batches
            .iter()
            .flat_map(|batch| batch.iter())
            .all(|p| p.meta().discard() == should_discard));
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
        assert!(batches
            .iter()
            .flat_map(|batch| batch.iter())
            .all(|p| p.meta().discard()));
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
        assert!(batches
            .iter()
            .flat_map(|batch| batch.iter())
            .zip(ref_vec.into_iter().flatten())
            .all(|(p, discard)| {
                if discard == 0 {
                    p.meta().discard()
                } else {
                    !p.meta().discard()
                }
            }));
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
                true,
            )
            .unwrap();
            assert!(!is_simple_vote_transaction_view(&view));
        }
    }

    #[test]
    fn test_shrink_fuzz() {
        let mut rng = rand::rng();
        for _ in 0..5 {
            let mut batches: Vec<_> = (0..3)
                .map(|_| {
                    if rng.random_bool(0.5) {
                        let batch = (0..PACKETS_PER_BATCH)
                            .map(|_| {
                                BytesPacket::from_data(None, test_tx()).expect("serialize request")
                            })
                            .collect::<BytesPacketBatch>();
                        PacketBatch::Bytes(batch)
                    } else {
                        let batch = (0..PACKETS_PER_BATCH)
                            .map(|_| Packet::from_data(None, test_tx()).expect("serialize request"))
                            .collect::<Vec<_>>();
                        PacketBatch::Pinned(RecycledPacketBatch::new(batch))
                    }
                })
                .collect();
            batches.iter_mut().for_each(|b| {
                b.iter_mut()
                    .for_each(|mut p| p.meta_mut().set_discard(rand::rng().random()))
            });
            //find all the non discarded packets
            let mut start = vec![];
            batches.iter_mut().for_each(|b| {
                b.iter_mut()
                    .filter(|p| !p.meta().discard())
                    .for_each(|p| start.push(p.data(..).unwrap().to_vec()))
            });
            start.sort();

            let packet_count = count_valid_packets(&batches);
            let mut batches = shrink_batches(batches);

            //make sure all the non discarded packets are the same
            let mut end = vec![];
            batches.iter_mut().for_each(|b| {
                b.iter_mut()
                    .filter(|p| !p.meta().discard())
                    .for_each(|p| end.push(p.data(..).unwrap().to_vec()))
            });
            end.sort();
            let packet_count2 = count_valid_packets(&batches);
            assert_eq!(packet_count, packet_count2);
            assert_eq!(start, end);
        }
    }

    #[test]
    fn test_shrink_empty() {
        const PACKET_COUNT: usize = 1024;
        const BATCH_COUNT: usize = PACKET_COUNT / PACKETS_PER_BATCH;

        // No batches
        // truncate of 1 on len 0 is a noop
        shrink_batches(Vec::new());
        // One empty batch
        {
            let batches = vec![RecycledPacketBatch::with_capacity(0).into()];
            let batches = shrink_batches(batches);
            assert_eq!(batches.len(), 0);
        }
        // Many empty batches
        {
            let batches = (0..BATCH_COUNT)
                .map(|_| RecycledPacketBatch::with_capacity(0).into())
                .collect::<Vec<_>>();
            let batches = shrink_batches(batches);
            assert_eq!(batches.len(), 0);
        }
    }

    #[test]
    fn test_shrink_vectors() {
        const PACKET_COUNT: usize = 1024;
        const BATCH_COUNT: usize = PACKET_COUNT / PACKETS_PER_BATCH;

        let set_discards = [
            // contiguous
            // 0
            // No discards
            |_, _| false,
            // All discards
            |_, _| true,
            // single partitions
            // discard last half of packets
            |b, p| ((b * PACKETS_PER_BATCH) + p) >= (PACKET_COUNT / 2),
            // discard first half of packets
            |b, p| ((b * PACKETS_PER_BATCH) + p) < (PACKET_COUNT / 2),
            // discard last half of each batch
            |_, p| p >= (PACKETS_PER_BATCH / 2),
            // 5
            // discard first half of each batch
            |_, p| p < (PACKETS_PER_BATCH / 2),
            // uniform sparse
            // discard even packets
            |b: usize, p: usize| ((b * PACKETS_PER_BATCH) + p).is_multiple_of(2),
            // discard odd packets
            |b: usize, p: usize| !((b * PACKETS_PER_BATCH) + p).is_multiple_of(2),
            // discard even batches
            |b, _| b % 2 == 0,
            // discard odd batches
            |b, _| b % 2 == 1,
            // edges
            // 10
            // discard first batch
            |b, _| b == 0,
            // discard last batch
            |b, _| b == BATCH_COUNT - 1,
            // discard first and last batches
            |b, _| b == 0 || b == BATCH_COUNT - 1,
            // discard all but first and last batches
            |b, _| b != 0 && b != BATCH_COUNT - 1,
            // discard first packet
            |b, p| ((b * PACKETS_PER_BATCH) + p) == 0,
            // 15
            // discard all but first packet
            |b, p| ((b * PACKETS_PER_BATCH) + p) != 0,
            // discard last packet
            |b, p| ((b * PACKETS_PER_BATCH) + p) == PACKET_COUNT - 1,
            // discard all but last packet
            |b, p| ((b * PACKETS_PER_BATCH) + p) != PACKET_COUNT - 1,
            // discard first packet of each batch
            |_, p| p == 0,
            // discard all but first packet of each batch
            |_, p| p != 0,
            // 20
            // discard last packet of each batch
            |_, p| p == PACKETS_PER_BATCH - 1,
            // discard all but last packet of each batch
            |_, p| p != PACKETS_PER_BATCH - 1,
            // discard first and last packet of each batch
            |_, p| p == 0 || p == PACKETS_PER_BATCH - 1,
            // discard all but first and last packet of each batch
            |_, p| p != 0 && p != PACKETS_PER_BATCH - 1,
            // discard all after first packet in second to last batch
            |b, p| (b == BATCH_COUNT - 2 && p > 0) || b == BATCH_COUNT - 1,
            // 25
        ];

        let expect_valids = [
            // (expected_batches, expected_valid_packets)
            //
            // contiguous
            // 0
            (BATCH_COUNT, PACKET_COUNT),
            (0, 0),
            // single partitions
            (BATCH_COUNT / 2, PACKET_COUNT / 2),
            (BATCH_COUNT / 2, PACKET_COUNT / 2),
            (BATCH_COUNT / 2, PACKET_COUNT / 2),
            // 5
            (BATCH_COUNT / 2, PACKET_COUNT / 2),
            // uniform sparse
            (BATCH_COUNT / 2, PACKET_COUNT / 2),
            (BATCH_COUNT / 2, PACKET_COUNT / 2),
            (BATCH_COUNT / 2, PACKET_COUNT / 2),
            (BATCH_COUNT / 2, PACKET_COUNT / 2),
            // edges
            // 10
            (BATCH_COUNT - 1, PACKET_COUNT - PACKETS_PER_BATCH),
            (BATCH_COUNT - 1, PACKET_COUNT - PACKETS_PER_BATCH),
            (BATCH_COUNT - 2, PACKET_COUNT - 2 * PACKETS_PER_BATCH),
            (2, 2 * PACKETS_PER_BATCH),
            (BATCH_COUNT, PACKET_COUNT - 1),
            // 15
            (1, 1),
            (BATCH_COUNT, PACKET_COUNT - 1),
            (1, 1),
            (
                (BATCH_COUNT * (PACKETS_PER_BATCH - 1) + PACKETS_PER_BATCH) / PACKETS_PER_BATCH,
                (PACKETS_PER_BATCH - 1) * BATCH_COUNT,
            ),
            (
                (BATCH_COUNT + PACKETS_PER_BATCH) / PACKETS_PER_BATCH,
                BATCH_COUNT,
            ),
            // 20
            (
                (BATCH_COUNT * (PACKETS_PER_BATCH - 1) + PACKETS_PER_BATCH) / PACKETS_PER_BATCH,
                (PACKETS_PER_BATCH - 1) * BATCH_COUNT,
            ),
            (
                (BATCH_COUNT + PACKETS_PER_BATCH) / PACKETS_PER_BATCH,
                BATCH_COUNT,
            ),
            (
                (BATCH_COUNT * (PACKETS_PER_BATCH - 2) + PACKETS_PER_BATCH) / PACKETS_PER_BATCH,
                (PACKETS_PER_BATCH - 2) * BATCH_COUNT,
            ),
            (
                (2 * BATCH_COUNT + PACKETS_PER_BATCH) / PACKETS_PER_BATCH,
                PACKET_COUNT - (PACKETS_PER_BATCH - 2) * BATCH_COUNT,
            ),
            (BATCH_COUNT - 1, PACKET_COUNT - 2 * PACKETS_PER_BATCH + 1),
            // 25
        ];

        let test_cases = set_discards.iter().zip(&expect_valids).enumerate();
        for (i, (set_discard, (expect_batch_count, expect_valid_packets))) in test_cases {
            debug!("test_shrink case: {i}");
            let mut batches = to_packet_batches(
                &(0..PACKET_COUNT).map(|_| test_tx()).collect::<Vec<_>>(),
                PACKETS_PER_BATCH,
            );
            assert_eq!(batches.len(), BATCH_COUNT);
            assert_eq!(count_valid_packets(&batches), PACKET_COUNT);
            batches.iter_mut().enumerate().for_each(|(i, b)| {
                b.iter_mut()
                    .enumerate()
                    .for_each(|(j, mut p)| p.meta_mut().set_discard(set_discard(i, j)))
            });
            assert_eq!(count_valid_packets(&batches), *expect_valid_packets);
            debug!("show valid packets for case {i}");
            batches.iter_mut().enumerate().for_each(|(i, b)| {
                b.iter_mut().enumerate().for_each(|(j, p)| {
                    if !p.meta().discard() {
                        trace!("{i} {j}")
                    }
                })
            });
            debug!("done show valid packets for case {i}");
            let batches = shrink_batches(batches);
            let shrunken_batch_count = batches.len();
            debug!("shrunk batch test {i} count: {shrunken_batch_count}");
            assert_eq!(shrunken_batch_count, *expect_batch_count);
            assert_eq!(count_valid_packets(&batches), *expect_valid_packets);
        }
    }

    #[test]
    fn test_split_batches() {
        let tx = test_tx();

        let batches = vec![];
        let (bytes_batches, pinned_batches) = split_batches(batches);
        assert!(bytes_batches.is_empty());
        assert!(pinned_batches.is_empty());

        let pinned_packet = Packet::from_data(None, tx.clone()).unwrap();
        let bytes_packet = BytesPacket::from_data(None, tx).unwrap();
        let batches = vec![
            PacketBatch::Pinned(RecycledPacketBatch::new(vec![pinned_packet.clone(); 10])),
            PacketBatch::Bytes(BytesPacketBatch::from(vec![bytes_packet.clone(); 10])),
            PacketBatch::Pinned(RecycledPacketBatch::new(vec![pinned_packet.clone(); 10])),
            PacketBatch::Pinned(RecycledPacketBatch::new(vec![pinned_packet.clone(); 10])),
            PacketBatch::Bytes(BytesPacketBatch::from(vec![bytes_packet.clone(); 10])),
        ];
        let (bytes_batches, pinned_batches) = split_batches(batches);
        assert_eq!(
            bytes_batches,
            vec![
                BytesPacketBatch::from(vec![bytes_packet.clone(); 10]),
                BytesPacketBatch::from(vec![bytes_packet; 10]),
            ]
        );
        assert_eq!(
            pinned_batches,
            vec![
                RecycledPacketBatch::new(vec![pinned_packet.clone(); 10]),
                RecycledPacketBatch::new(vec![pinned_packet.clone(); 10]),
                RecycledPacketBatch::new(vec![pinned_packet; 10]),
            ]
        )
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
