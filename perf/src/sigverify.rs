//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.
use {
    crate::packet::{PacketBatch, PacketFlags, PacketRefMut},
    agave_transaction_view::{
        transaction_data::TransactionData, transaction_version::TransactionVersion,
        transaction_view::SanitizedTransactionView,
    },
    rayon::prelude::*,
    solana_runtime_transaction::sanitize_config::sanitize_config,
};

// Empirically derived to constrain max verify latency to ~8ms at lower packet counts
pub const VERIFY_PACKET_CHUNK_SIZE: usize = 128;

/// Returns true if the signature on the packet verifies.
/// Caller must do packet.set_discard(true) if this returns false.
#[must_use]
fn verify_packet(packet: &mut PacketRefMut, reject_non_vote: bool, enable_tx_v1: bool) -> bool {
    // If this packet was already marked as discard, drop it
    if packet.meta().discard() {
        return false;
    }

    let Some(data) = packet.data(..) else {
        return false;
    };

    let (is_simple_vote_tx, verified) = {
        let Ok(view) = SanitizedTransactionView::try_new_sanitized(data, &sanitize_config(true))
        else {
            return false;
        };

        if !enable_tx_v1 && matches!(view.version(), TransactionVersion::V1) {
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
    enable_tx_v1: bool,
) {
    debug!("CPU ECDSA for {packet_count}");
    thread_pool.install(|| {
        batches.par_iter_mut().flatten().for_each(|mut packet| {
            if !packet.meta().discard()
                && !verify_packet(&mut packet, reject_non_vote, enable_tx_v1)
            {
                packet.meta_mut().set_discard(true);
            }
        });
    });
}

/// Verifies every packet across all of `batches` on the current thread.
#[cfg(not(feature = "ed25519-simd-verify"))]
pub fn ed25519_verify_serial(
    batches: &mut [PacketBatch],
    reject_non_vote: bool,
    enable_tx_v1: bool,
) {
    for mut packet in batches.iter_mut().flatten() {
        if !packet.meta().discard() && !verify_packet(&mut packet, reject_non_vote, enable_tx_v1) {
            packet.meta_mut().set_discard(true);
        }
    }
}

/// AVX-512-backed `ed25519_verify_serial` for the experimental feature.
#[cfg(feature = "ed25519-simd-verify")]
pub fn ed25519_verify_serial(
    batches: &mut [PacketBatch],
    reject_non_vote: bool,
    enable_tx_v1: bool,
) {
    simd_verify::ed25519_verify_serial(batches, reject_non_vote, enable_tx_v1)
}

#[cfg(feature = "ed25519-simd-verify")]
mod simd_verify {
    use {
        super::{
            PacketBatch, PacketFlags, TransactionVersion, is_simple_vote_transaction_view,
            sanitize_config,
        },
        agave_transaction_view::transaction_view::SanitizedTransactionView,
        ed25519_simd::{Verifier, VerifyInput, VerifyPolicy},
        solana_signature::Signature,
        std::cell::RefCell,
    };

    thread_local! {
        // Match the non-SIMD `verify_strict` policy.
        static VERIFIER: RefCell<Verifier> = RefCell::new(Verifier::with_policy(VerifyPolicy::Dalek));
    }

    // SIMD setup is slower for a single signature.
    const SIMD_BATCH_THRESHOLD: usize = 2;

    struct PendingSig {
        public_key: [u8; 32],
        signature: [u8; 64],
        message_start: usize,
        message_end: usize,
    }

    enum PacketSummary {
        Discard,
        Verify {
            is_simple_vote_tx: bool,
            sig_start: usize,
            sig_end: usize,
        },
    }

    pub(super) fn ed25519_verify_serial(
        batches: &mut [PacketBatch],
        reject_non_vote: bool,
        enable_tx_v1: bool,
    ) {
        let mut summaries = Vec::with_capacity(batches.iter().map(PacketBatch::len).sum());
        let mut pending: Vec<PendingSig> = Vec::new();
        let mut message_arena: Vec<u8> = Vec::new();

        for packet in batches.iter().flatten() {
            if packet.meta().discard() {
                summaries.push(PacketSummary::Discard);
                continue;
            }

            let Some(data) = packet.data(..) else {
                summaries.push(PacketSummary::Discard);
                continue;
            };

            let Ok(view) =
                SanitizedTransactionView::try_new_sanitized(data, &sanitize_config(true))
            else {
                summaries.push(PacketSummary::Discard);
                continue;
            };

            if !enable_tx_v1 && matches!(view.version(), TransactionVersion::V1) {
                summaries.push(PacketSummary::Discard);
                continue;
            }

            let is_simple_vote_tx = is_simple_vote_transaction_view(&view);
            if reject_non_vote && !is_simple_vote_tx {
                summaries.push(PacketSummary::Discard);
                continue;
            }

            let signatures = view.signatures();
            if signatures.is_empty() {
                summaries.push(PacketSummary::Discard);
                continue;
            }

            let message_start = message_arena.len();
            message_arena.extend_from_slice(view.message_data());
            let message_end = message_arena.len();

            let sig_start = pending.len();
            for (signature, pubkey) in signatures.iter().zip(view.static_account_keys()) {
                pending.push(PendingSig {
                    public_key: pubkey.to_bytes(),
                    signature: *signature.as_array(),
                    message_start,
                    message_end,
                });
            }
            summaries.push(PacketSummary::Verify {
                is_simple_vote_tx,
                sig_start,
                sig_end: pending.len(),
            });
        }

        let mut results = vec![false; pending.len()];
        if pending.len() < SIMD_BATCH_THRESHOLD {
            for (result, p) in results.iter_mut().zip(&pending) {
                *result = Signature::from(p.signature)
                    .verify(&p.public_key, &message_arena[p.message_start..p.message_end]);
            }
        } else if !pending.is_empty() {
            let verify_inputs: Vec<VerifyInput> = pending
                .iter()
                .map(|p| VerifyInput {
                    public_key: p.public_key,
                    signature: p.signature,
                    message: &message_arena[p.message_start..p.message_end],
                })
                .collect();
            VERIFIER.with(|verifier| {
                verifier.borrow_mut().verify_batch(&verify_inputs, &mut results);
            });
        }

        for (mut packet, summary) in batches.iter_mut().flatten().zip(summaries.iter()) {
            match summary {
                PacketSummary::Discard => packet.meta_mut().set_discard(true),
                PacketSummary::Verify {
                    is_simple_vote_tx,
                    sig_start,
                    sig_end,
                } => {
                    if *is_simple_vote_tx {
                        packet.meta_mut().flags |= PacketFlags::SIMPLE_VOTE_TX;
                    }
                    if !results[*sig_start..*sig_end].iter().all(|&ok| ok) {
                        packet.meta_mut().set_discard(true);
                    }
                }
            }
        }
    }
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
        solana_system_interface::instruction as system_instruction,
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

    fn test_tx_v1() -> VersionedTransaction {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let instruction = system_instruction::transfer(&payer.pubkey(), &recipient, 1);
        let message = solana_message::v1::Message::try_compile(
            &payer.pubkey(),
            &[instruction],
            Hash::new_unique(),
        )
        .unwrap();

        VersionedTransaction::try_new(VersionedMessage::V1(message), &[&payer]).unwrap()
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
        BytesPacket::from_data(tx).unwrap()
    }

    #[test]
    fn test_untrustworthy_sigs() {
        let required_num_sigs = 14;
        let actual_num_sigs = 5;

        let mut packet = packet_from_num_sigs(required_num_sigs, actual_num_sigs);
        assert!(!sigverify::verify_packet(
            &mut packet.as_mut(),
            false,
            false
        ));
    }

    #[test]
    fn test_small_packet() {
        let tx = test_tx();
        let mut data = bincode::serialize(&tx).unwrap();

        data[0] = 0xff;
        data[1] = 0xff;
        data.truncate(2);

        let mut packet = BytesPacket::from_bytes(None, Bytes::from(data));
        assert!(!sigverify::verify_packet(
            &mut packet.as_mut(),
            false,
            false
        ));
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
        let mut packet = BytesPacket::from_data(tx).unwrap();

        assert!(!verify_packet(&mut packet.as_mut(), false, false));

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

        let mut packet = BytesPacket::from_data(tx).unwrap();

        assert!(!verify_packet(&mut packet.as_mut(), false, false));

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
        assert!(!sigverify::verify_packet(
            &mut packet.as_mut(),
            false,
            false
        ));
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
        assert!(!sigverify::verify_packet(
            &mut packet.as_mut(),
            false,
            false
        ));
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
        assert!(!sigverify::verify_packet(
            &mut packet.as_mut(),
            false,
            false
        ));
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
        let mut packet = BytesPacket::from_data(tx).unwrap();
        assert!(!sigverify::verify_packet(
            &mut packet.as_mut(),
            false,
            false
        ));
    }

    #[test]
    fn test_unsupported_version() {
        let tx = test_tx();
        let mut data = bincode::serialize(&tx).unwrap();

        // Set message version to 2. V1 is supported by transaction-view, but
        // still explicitly feature-gated by sigverify.
        const MESSAGE_OFFSET: usize = 1 + core::mem::size_of::<Signature>();
        data[MESSAGE_OFFSET] = MESSAGE_VERSION_PREFIX + 2;

        let mut packet = BytesPacket::from_bytes(None, Bytes::from(data));
        assert!(!sigverify::verify_packet(
            &mut packet.as_mut(),
            false,
            false
        ));
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
        sigverify::ed25519_verify(&threadpool, batches, false, packet_count, false);
    }

    #[test]
    fn test_verify_tampered_sig_len() {
        let mut tx = test_tx();
        // pretend malicious leader dropped a signature...
        tx.signatures.pop();
        let packet = BytesPacket::from_data(tx).unwrap();

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
    fn test_verify_serial() {
        agave_logger::setup();

        let valid_simple = test_tx();
        let valid_multisig = test_multisig_tx();
        let mut tampered_multisig_data = bincode::serialize(&valid_multisig).unwrap();
        tampered_multisig_data[40] = tampered_multisig_data[40].wrapping_add(8);

        let mut packet_batch = BytesPacketBatch::with_capacity(4);
        packet_batch.push(BytesPacket::from_data(&valid_simple).unwrap());
        packet_batch.push(BytesPacket::from_data(&valid_multisig).unwrap());
        packet_batch.push(BytesPacket::from_bytes(
            None,
            Bytes::from(tampered_multisig_data),
        ));
        let mut already_discarded = BytesPacket::from_data(&valid_simple).unwrap();
        already_discarded.meta_mut().set_discard(true);
        packet_batch.push(already_discarded);

        let mut batch = PacketBatch::from(packet_batch);
        ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);

        let discards: Vec<bool> = batch.iter().map(|p| p.meta().discard()).collect();
        assert_eq!(discards, vec![false, false, true, true]);
    }

    #[test]
    fn test_verify_serial_single_signature() {
        // Exercises the scalar fallback path.
        agave_logger::setup();

        let mut tx = test_tx();
        let mut packet_batch = BytesPacketBatch::with_capacity(1);
        packet_batch.push(BytesPacket::from_data(&tx).unwrap());
        let mut batch = PacketBatch::from(packet_batch);
        ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
        assert!(!batch.get(0).unwrap().meta().discard());

        tx.signatures[0] = solana_signature::Signature::default();
        let mut packet_batch = BytesPacketBatch::with_capacity(1);
        packet_batch.push(BytesPacket::from_data(&tx).unwrap());
        let mut batch = PacketBatch::from(packet_batch);
        ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
        assert!(batch.get(0).unwrap().meta().discard());
    }

    #[test]
    fn test_verify_serial_across_multiple_batches() {
        // Verify separate source batches in one call.
        agave_logger::setup();

        let valid_tx = test_tx();
        let mut tampered_data = bincode::serialize(&valid_tx).unwrap();
        // Corrupt the signature while keeping the transaction parseable.
        tampered_data[5] ^= 0xff;

        let mut batches: Vec<PacketBatch> = (0..8)
            .map(|i| {
                let mut packet_batch = BytesPacketBatch::with_capacity(1);
                if i % 2 == 0 {
                    packet_batch.push(BytesPacket::from_data(&valid_tx).unwrap());
                } else {
                    packet_batch.push(BytesPacket::from_bytes(
                        None,
                        Bytes::from(tampered_data.clone()),
                    ));
                }
                PacketBatch::from(packet_batch)
            })
            .collect();

        ed25519_verify_serial(&mut batches, false, false);

        for (i, batch) in batches.iter().enumerate() {
            let discarded = batch.get(0).unwrap().meta().discard();
            assert_eq!(discarded, i % 2 != 0, "batch {i}");
        }
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
            let packet = BytesPacket::from_data(tx).unwrap();
            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                &sanitize_config(true),
            )
            .unwrap();
            assert!(!is_simple_vote_transaction_view(&view));
        }

        // single legacy vote tx is
        {
            let mut tx = new_test_vote_tx(&mut rng);
            tx.message.instructions[0].data = vec![1, 2, 3];
            let packet = BytesPacket::from_data(tx).unwrap();
            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                &sanitize_config(true),
            )
            .unwrap();
            assert!(is_simple_vote_transaction_view(&view));
        }

        // single versioned vote tx is not
        {
            let tx = new_test_vote_tx_v0();
            let packet = BytesPacket::from_data(tx).unwrap();

            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                &sanitize_config(true),
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
            let packet = BytesPacket::from_data(tx).unwrap();
            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                &sanitize_config(true),
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
            let packet = BytesPacket::from_data(tx).unwrap();
            let view = SanitizedTransactionView::try_new_sanitized(
                packet.as_ref().data(..).unwrap(),
                &sanitize_config(true),
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
            BytesPacket::from_data(tx.clone()).unwrap()
        } else {
            let tx: Transaction = new_test_tx_with_number_of_ixs(number_of_ixs);
            BytesPacket::from_data(tx.clone()).unwrap()
        };

        assert_eq!(
            sigverify::verify_packet(&mut packet.as_mut(), false, false),
            !too_many_ixs
        );
    }

    #[test_case(false, false; "tx_v1_disabled")]
    #[test_case(true, true; "tx_v1_enabled")]
    fn test_verify_packet_tx_v1_feature_gate(enable_tx_v1: bool, expected: bool) {
        let tx = test_tx_v1();
        let mut packet = BytesPacket::from_bytes(None, wincode::serialize(&tx).unwrap());

        assert_eq!(
            verify_packet(&mut packet.as_mut(), false, enable_tx_v1),
            expected,
        );
    }
}
