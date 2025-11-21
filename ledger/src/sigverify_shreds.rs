#![allow(clippy::implicit_hasher)]
use {
    crate::shred,
    rayon::{prelude::*, ThreadPool},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_metrics::inc_new_counter_debug,
    solana_nohash_hasher::BuildNoHashHasher,
    solana_perf::{
        packet::{PacketBatch, PacketRef},
        sigverify::count_packets_in_batches,
    },
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{collections::HashMap, sync::RwLock},
};
#[cfg(test)]
use {solana_keypair::Keypair, solana_perf::packet::PacketRefMut, solana_signer::Signer};

pub type LruCache = lazy_lru::LruCache<(Signature, Pubkey, /*merkle root:*/ Hash), ()>;

pub type SlotPubkeys = HashMap<Slot, Pubkey, BuildNoHashHasher<Slot>>;

#[must_use]
pub fn verify_shred_cpu(
    packet: PacketRef,
    slot_leaders: &SlotPubkeys,
    cache: &RwLock<LruCache>,
) -> bool {
    if packet.meta().discard() {
        return false;
    }
    let Some(shred) = shred::layout::get_shred(packet) else {
        return false;
    };
    let Some(slot) = shred::layout::get_slot(shred) else {
        return false;
    };
    trace!("slot {slot}");
    let Some(pubkey) = slot_leaders.get(&slot) else {
        return false;
    };
    let Some(signature) = shred::layout::get_signature(shred) else {
        return false;
    };
    trace!("signature {signature}");
    let Some(data) = shred::layout::get_signed_data(shred) else {
        return false;
    };

    let key = (signature, *pubkey, data);
    if cache.read().unwrap().get(&key).is_some() {
        true
    } else if key.0.verify(key.1.as_ref(), key.2.as_ref()) {
        cache.write().unwrap().put(key, ());
        true
    } else {
        false
    }
}

pub fn verify_shreds(
    thread_pool: &ThreadPool,
    batches: &[PacketBatch],
    slot_leaders: &SlotPubkeys,
    cache: &RwLock<LruCache>,
) -> Vec<Vec<u8>> {
    let packet_count = count_packets_in_batches(batches);
    debug!("CPU SHRED ECDSA for {packet_count}");
    let rv = thread_pool.install(|| {
        batches
            .into_par_iter()
            .map(|batch| {
                batch
                    .par_iter()
                    .map(|packet| u8::from(verify_shred_cpu(packet, slot_leaders, cache)))
                    .collect()
            })
            .collect()
    });
    inc_new_counter_debug!("ed25519_shred_verify_cpu", packet_count);
    rv
}

#[cfg(test)]
fn sign_shred_cpu(keypair: &Keypair, packet: &mut PacketRefMut) {
    let sig = shred::layout::get_signature_range();
    let msg = shred::layout::get_shred(packet.as_ref())
        .and_then(shred::layout::get_signed_data)
        .unwrap();
    assert!(
        packet.meta().size >= sig.end,
        "packet is not large enough for a signature"
    );
    let signature = keypair.sign_message(msg.as_ref());
    trace!("signature {signature:?}");
    let mut buffer = packet
        .data(..)
        .expect("packet should not be discarded")
        .to_vec();
    buffer[sig].copy_from_slice(signature.as_ref());
    packet.copy_from_slice(&buffer);
}

#[cfg(test)]
fn sign_shreds(thread_pool: &ThreadPool, keypair: &Keypair, batches: &mut [PacketBatch]) {
    let packet_count = count_packets_in_batches(batches);
    debug!("CPU SHRED ECDSA for {packet_count}");
    thread_pool.install(|| {
        batches.par_iter_mut().for_each(|batch| {
            batch
                .par_iter_mut()
                .for_each(|mut p| sign_shred_cpu(keypair, &mut p));
        });
    });
    inc_new_counter_debug!("ed25519_shred_sign_cpu", packet_count);
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            shred::{ProcessShredsStats, Shred},
            shredder::{ReedSolomonCache, Shredder},
        },
        assert_matches::assert_matches,
        itertools::Itertools,
        rand::{seq::SliceRandom, Rng},
        rayon::ThreadPoolBuilder,
        solana_entry::entry::Entry,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_packet::Packet,
        solana_perf::packet::RecycledPacketBatch,
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        solana_transaction::Transaction,
        std::iter::{once, repeat_with},
        test_case::test_case,
    };

    fn run_test_sigverify_shred_cpu(slot: Slot) {
        agave_logger::setup();
        let mut packet = Packet::default();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 0).unwrap();
        let keypair = Keypair::new();
        let reed_solomon_cache = ReedSolomonCache::default();
        let (mut shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &[],
            true,
            Hash::default(),
            0,
            0,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );
        let shred = shreds.pop().unwrap();
        assert_eq!(shred.slot(), slot);
        trace!("signature {}", shred.signature());
        packet.buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        packet.meta_mut().size = shred.payload().len();

        let leader_slots: SlotPubkeys = [(slot, keypair.pubkey())].into_iter().collect();
        assert!(verify_shred_cpu((&packet).into(), &leader_slots, &cache));

        let wrong_keypair = Keypair::new();
        let leader_slots: SlotPubkeys = [(slot, wrong_keypair.pubkey())].into_iter().collect();
        assert!(!verify_shred_cpu((&packet).into(), &leader_slots, &cache));

        let leader_slots: SlotPubkeys = HashMap::default();
        assert!(!verify_shred_cpu((&packet).into(), &leader_slots, &cache));
    }

    #[test]
    fn test_sigverify_shred_cpu() {
        run_test_sigverify_shred_cpu(0xdead_c0de);
    }

    fn run_test_sigverify_shreds_cpu(thread_pool: &ThreadPool, slot: Slot) {
        agave_logger::setup();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let keypair = Keypair::new();
        let batch = make_packet_batch(&keypair, slot);
        let mut batches = [batch];

        let leader_slots: SlotPubkeys = [(slot, keypair.pubkey())].into_iter().collect();
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 1);

        let wrong_keypair = Keypair::new();
        let leader_slots: SlotPubkeys = [(slot, wrong_keypair.pubkey())].into_iter().collect();
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);

        let leader_slots: SlotPubkeys = HashMap::default();
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);

        let leader_slots: SlotPubkeys = [(slot, keypair.pubkey())].into_iter().collect();
        batches[0]
            .iter_mut()
            .for_each(|mut packet_ref| packet_ref.meta_mut().size = 0);
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);
    }

    #[test]
    fn test_sigverify_shreds_cpu() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        run_test_sigverify_shreds_cpu(&thread_pool, 0xdead_c0de);
    }

    fn run_test_sigverify_shreds(thread_pool: &ThreadPool, slot: Slot) {
        agave_logger::setup();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));

        let keypair = Keypair::new();
        let batch = make_packet_batch(&keypair, slot);
        let mut batches = [batch];

        let leader_slots: SlotPubkeys = [(u64::MAX, Pubkey::default()), (slot, keypair.pubkey())]
            .into_iter()
            .collect();
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 1);

        let wrong_keypair = Keypair::new();
        let leader_slots: SlotPubkeys = [
            (u64::MAX, Pubkey::default()),
            (slot, wrong_keypair.pubkey()),
        ]
        .into_iter()
        .collect();
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);

        let leader_slots: SlotPubkeys = [(u64::MAX, Pubkey::default())].into_iter().collect();
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);

        batches[0]
            .iter_mut()
            .for_each(|mut pr| pr.meta_mut().size = 0);
        let leader_slots: SlotPubkeys = [(u64::MAX, Pubkey::default()), (slot, keypair.pubkey())]
            .into_iter()
            .collect();
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);
    }

    fn make_packet_batch(keypair: &Keypair, slot: u64) -> PacketBatch {
        let mut batch = RecycledPacketBatch::default();
        let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 0).unwrap();
        let reed_solomon_cache = ReedSolomonCache::default();
        let (shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            keypair,
            &[],
            true,
            Hash::default(),
            0,
            0,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );
        batch.resize(shreds.len(), Packet::default());
        for i in 0..shreds.len() {
            batch[i].buffer_mut()[..shreds[i].payload().len()].copy_from_slice(shreds[i].payload());
            batch[i].meta_mut().size = shreds[i].payload().len();
        }
        PacketBatch::from(batch)
    }

    #[test]
    fn test_sigverify_shreds() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        run_test_sigverify_shreds(&thread_pool, 0xdead_c0de);
    }

    fn make_transaction<R: Rng>(rng: &mut R) -> Transaction {
        let block = rng.random::<[u8; 32]>();
        let recent_blockhash = solana_sha256_hasher::hashv(&[&block]);
        system_transaction::transfer(
            &Keypair::new(),       // from
            &Pubkey::new_unique(), // to
            rng.random(),          // lamports
            recent_blockhash,
        )
    }

    fn make_entry<R: Rng>(rng: &mut R, prev_hash: &Hash) -> Entry {
        let size = rng.random_range(16..32);
        let txs = repeat_with(|| make_transaction(rng)).take(size).collect();
        Entry::new(
            prev_hash,
            rng.random_range(1..64), // num_hashes
            txs,
        )
    }

    fn make_entries<R: Rng>(rng: &mut R, num_entries: usize) -> Vec<Entry> {
        let prev_hash = solana_sha256_hasher::hashv(&[&rng.random::<[u8; 32]>()]);
        let entry = make_entry(rng, &prev_hash);
        std::iter::successors(Some(entry), |entry| Some(make_entry(rng, &entry.hash)))
            .take(num_entries)
            .collect()
    }

    fn make_shreds<R: Rng>(
        rng: &mut R,
        is_last_in_slot: bool,
        keypairs: &HashMap<Slot, Keypair>,
    ) -> Vec<Shred> {
        let reed_solomon_cache = ReedSolomonCache::default();
        let mut shreds: Vec<_> = keypairs
            .iter()
            .flat_map(|(&slot, keypair)| {
                let parent_slot = slot - rng.random::<u16>().max(1) as Slot;
                let num_entries = rng.random_range(64..128);
                Shredder::new(
                    slot,
                    parent_slot,
                    rng.random_range(0..0x40), // reference_tick
                    rng.random(),              // version
                )
                .unwrap()
                .make_merkle_shreds_from_entries(
                    keypair,
                    &make_entries(rng, num_entries),
                    is_last_in_slot,
                    Hash::new_from_array(rng.random()), // chained_merkle_root
                    rng.random_range(0..2671),          // next_shred_index
                    rng.random_range(0..2781),          // next_code_index
                    &reed_solomon_cache,
                    &mut ProcessShredsStats::default(),
                )
            })
            .collect();
        shreds.shuffle(rng);
        // Assert that all shreds verify and sanitize.
        for shred in &shreds {
            let pubkey = keypairs[&shred.slot()].pubkey();
            assert!(shred.verify(&pubkey));
            assert_matches!(shred.sanitize(), Ok(()));
        }
        // Verify using layout api.
        for shred in &shreds {
            let shred = shred.payload();
            let slot = shred::layout::get_slot(shred).unwrap();
            let signature = shred::layout::get_signature(shred).unwrap();
            let pubkey = keypairs[&slot].pubkey();
            let data = shred::layout::get_signed_data(shred).unwrap();
            assert!(signature.verify(pubkey.as_ref(), data.as_ref()));
        }
        shreds
    }

    fn make_packets<R: Rng>(rng: &mut R, shreds: &[Shred]) -> Vec<PacketBatch> {
        let mut packets = shreds.iter().map(|shred| {
            let mut packet = Packet::default();
            shred.copy_to_packet(&mut packet);
            packet
        });
        let packets: Vec<PacketBatch> = repeat_with(|| {
            let size = rng.random_range(0..16);
            let packets = packets.by_ref().take(size).collect();
            let batch = RecycledPacketBatch::new(packets);
            (size == 0 || !batch.is_empty()).then_some(batch.into())
        })
        .while_some()
        .collect();
        assert_eq!(
            shreds.len(),
            packets.iter().map(|batch| batch.len()).sum::<usize>()
        );
        packets
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_verify_shreds_fuzz(is_last_in_slot: bool) {
        let mut rng = rand::rng();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let keypairs = repeat_with(|| rng.random_range(169_367_809..169_906_789))
            .map(|slot| (slot, Keypair::new()))
            .take(3)
            .collect();
        let shreds = make_shreds(&mut rng, is_last_in_slot, &keypairs);
        let pubkeys: SlotPubkeys = keypairs
            .iter()
            .map(|(&slot, keypair)| (slot, keypair.pubkey()))
            .chain(once((Slot::MAX, Pubkey::default())))
            .collect();
        let mut packets = make_packets(&mut rng, &shreds);
        assert_eq!(
            verify_shreds(&thread_pool, &packets, &pubkeys, &cache),
            packets
                .iter()
                .map(|batch| vec![1u8; batch.len()])
                .collect::<Vec<_>>()
        );
        // Invalidate signatures for a random number of packets.
        let out: Vec<_> = packets
            .iter_mut()
            .map(|packets| {
                let PacketBatch::Pinned(packets) = packets else {
                    unreachable!()
                };
                packets
                    .iter_mut()
                    .map(|packet| {
                        let coin_flip: bool = rng.random();
                        if !coin_flip {
                            shred::layout::corrupt_packet(&mut rng, packet, &keypairs);
                        }
                        u8::from(coin_flip)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(verify_shreds(&thread_pool, &packets, &pubkeys, &cache), out);
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_sign_shreds(is_last_in_slot: bool) {
        let mut rng = rand::rng();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let shreds = {
            let keypairs = repeat_with(|| rng.random_range(169_367_809..169_906_789))
                .map(|slot| (slot, Keypair::new()))
                .take(3)
                .collect();
            make_shreds(&mut rng, is_last_in_slot, &keypairs)
        };
        let keypair = Keypair::new();
        let pubkeys: SlotPubkeys = {
            let pubkey = keypair.pubkey();
            shreds
                .iter()
                .map(Shred::slot)
                .map(|slot| (slot, pubkey))
                .chain(once((Slot::MAX, Pubkey::default())))
                .collect()
        };
        let mut packets = make_packets(&mut rng, &shreds);
        // Assert that initially all signatures are invalid.
        assert_eq!(
            verify_shreds(&thread_pool, &packets, &pubkeys, &cache),
            packets
                .iter()
                .map(|batch| vec![0u8; batch.len()])
                .collect::<Vec<_>>()
        );
        // Sign and verify shreds signatures.
        sign_shreds(&thread_pool, &keypair, &mut packets);
        assert_eq!(
            verify_shreds(&thread_pool, &packets, &pubkeys, &cache),
            packets
                .iter()
                .map(|batch| vec![1u8; batch.len()])
                .collect::<Vec<_>>()
        );
    }
}
