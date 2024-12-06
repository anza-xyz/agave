#![allow(clippy::implicit_hasher)]
use {
    crate::shred::{self, SignedData},
    rayon::{prelude::*, ThreadPool},
    solana_metrics::inc_new_counter_debug,
    solana_perf::{
        packet::{Packet, PacketBatch},
        sigverify::count_packets_in_batches,
    },
    solana_sdk::{
        clock::Slot,
        hash::Hash,
        pubkey::Pubkey,
        signature::{Keypair, Signature, Signer},
    },
    std::{collections::HashMap, sync::RwLock},
};

pub type LruCache = lazy_lru::LruCache<(Signature, Pubkey, /*merkle root:*/ Hash), ()>;

#[must_use]
pub fn verify_shred_cpu(
    packet: &Packet,
    slot_leaders: &HashMap<Slot, Pubkey>,
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
    trace!("slot {}", slot);
    let Some(pubkey) = slot_leaders.get(&slot) else {
        return false;
    };
    let Some(signature) = shred::layout::get_signature(shred) else {
        return false;
    };
    trace!("signature {}", signature);
    let Some(data) = shred::layout::get_signed_data(shred) else {
        return false;
    };
    match data {
        SignedData::Chunk(chunk) => signature.verify(pubkey.as_ref(), chunk),
        SignedData::MerkleRoot(root) => {
            let key = (signature, *pubkey, root);
            if cache.read().unwrap().get(&key).is_some() {
                true
            } else if key.0.verify(key.1.as_ref(), key.2.as_ref()) {
                cache.write().unwrap().put(key, ());
                true
            } else {
                false
            }
        }
    }
}

fn verify_shreds_cpu(
    thread_pool: &ThreadPool,
    batches: &[PacketBatch],
    slot_leaders: &HashMap<Slot, Pubkey>,
    cache: &RwLock<LruCache>,
) -> Vec<Vec<u8>> {
    let packet_count = count_packets_in_batches(batches);
    debug!("CPU SHRED ECDSA for {}", packet_count);
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

pub fn verify_shreds(
    thread_pool: &ThreadPool,
    batches: &[PacketBatch],
    slot_leaders: &HashMap<Slot, Pubkey>,
    cache: &RwLock<LruCache>,
) -> Vec<Vec<u8>> {
    verify_shreds_cpu(thread_pool, batches, slot_leaders, cache)
}

fn sign_shred_cpu(keypair: &Keypair, packet: &mut Packet) {
    let sig = shred::layout::get_signature_range();
    let msg = shred::layout::get_shred(packet)
        .and_then(shred::layout::get_signed_data)
        .unwrap();
    assert!(
        packet.meta().size >= sig.end,
        "packet is not large enough for a signature"
    );
    let signature = keypair.sign_message(msg.as_ref());
    trace!("signature {:?}", signature);
    packet.buffer_mut()[sig].copy_from_slice(signature.as_ref());
}

pub fn sign_shreds_cpu(thread_pool: &ThreadPool, keypair: &Keypair, batches: &mut [PacketBatch]) {
    let packet_count = count_packets_in_batches(batches);
    debug!("CPU SHRED ECDSA for {}", packet_count);
    thread_pool.install(|| {
        batches.par_iter_mut().for_each(|batch| {
            batch[..]
                .par_iter_mut()
                .for_each(|p| sign_shred_cpu(keypair, p));
        });
    });
    inc_new_counter_debug!("ed25519_shred_sign_cpu", packet_count);
}

pub fn sign_shreds(thread_pool: &ThreadPool, keypair: &Keypair, batches: &mut [PacketBatch]) {
    sign_shreds_cpu(thread_pool, keypair, batches)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            shred::{ProcessShredsStats, Shred, ShredFlags, LEGACY_SHRED_DATA_CAPACITY},
            shredder::{ReedSolomonCache, Shredder},
        },
        assert_matches::assert_matches,
        itertools::Itertools,
        rand::{seq::SliceRandom, Rng},
        rayon::ThreadPoolBuilder,
        solana_entry::entry::Entry,
        solana_sdk::{
            hash::{self, Hash},
            signature::{Keypair, Signer},
            system_transaction,
            transaction::Transaction,
        },
        std::iter::{once, repeat_with},
        test_case::test_case,
    };

    fn run_test_sigverify_shred_cpu(slot: Slot) {
        solana_logger::setup();
        let mut packet = Packet::default();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let mut shred = Shred::new_from_data(
            slot,
            0xc0de,
            0xdead,
            &[1, 2, 3, 4],
            ShredFlags::LAST_SHRED_IN_SLOT,
            0,
            0,
            0xc0de,
        );
        assert_eq!(shred.slot(), slot);
        let keypair = Keypair::new();
        shred.sign(&keypair);
        trace!("signature {}", shred.signature());
        packet.buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        packet.meta_mut().size = shred.payload().len();

        let leader_slots = HashMap::from([(slot, keypair.pubkey())]);
        assert!(verify_shred_cpu(&packet, &leader_slots, &cache));

        let wrong_keypair = Keypair::new();
        let leader_slots = HashMap::from([(slot, wrong_keypair.pubkey())]);
        assert!(!verify_shred_cpu(&packet, &leader_slots, &cache));

        let leader_slots = HashMap::new();
        assert!(!verify_shred_cpu(&packet, &leader_slots, &cache));
    }

    #[test]
    fn test_sigverify_shred_cpu() {
        run_test_sigverify_shred_cpu(0xdead_c0de);
    }

    fn run_test_sigverify_shreds_cpu(thread_pool: &ThreadPool, slot: Slot) {
        solana_logger::setup();
        let mut batches = [PacketBatch::default()];
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let mut shred = Shred::new_from_data(
            slot,
            0xc0de,
            0xdead,
            &[1, 2, 3, 4],
            ShredFlags::LAST_SHRED_IN_SLOT,
            0,
            0,
            0xc0de,
        );
        let keypair = Keypair::new();
        shred.sign(&keypair);
        batches[0].resize(1, Packet::default());
        batches[0][0].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batches[0][0].meta_mut().size = shred.payload().len();

        let leader_slots = HashMap::from([(slot, keypair.pubkey())]);
        let rv = verify_shreds_cpu(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv, vec![vec![1]]);

        let wrong_keypair = Keypair::new();
        let leader_slots = HashMap::from([(slot, wrong_keypair.pubkey())]);
        let rv = verify_shreds_cpu(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv, vec![vec![0]]);

        let leader_slots = HashMap::new();
        let rv = verify_shreds_cpu(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv, vec![vec![0]]);

        let leader_slots = HashMap::from([(slot, keypair.pubkey())]);
        batches[0][0].meta_mut().size = 0;
        let rv = verify_shreds_cpu(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv, vec![vec![0]]);
    }

    #[test]
    fn test_sigverify_shreds_cpu() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        run_test_sigverify_shreds_cpu(&thread_pool, 0xdead_c0de);
    }

    fn run_test_sigverify_shreds(thread_pool: &ThreadPool, slot: Slot) {
        solana_logger::setup();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));

        let mut batches = [PacketBatch::default()];
        let mut shred = Shred::new_from_data(
            slot,
            0xc0de,
            0xdead,
            &[1, 2, 3, 4],
            ShredFlags::LAST_SHRED_IN_SLOT,
            0,
            0,
            0xc0de,
        );
        let keypair = Keypair::new();
        shred.sign(&keypair);
        batches[0].resize(1, Packet::default());
        batches[0][0].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batches[0][0].meta_mut().size = shred.payload().len();

        let leader_slots = HashMap::from([(u64::MAX, Pubkey::default()), (slot, keypair.pubkey())]);
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv, vec![vec![1]]);

        let wrong_keypair = Keypair::new();
        let leader_slots = HashMap::from([
            (u64::MAX, Pubkey::default()),
            (slot, wrong_keypair.pubkey()),
        ]);
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv, vec![vec![0]]);

        let leader_slots = HashMap::from([(u64::MAX, Pubkey::default())]);
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv, vec![vec![0]]);

        batches[0][0].meta_mut().size = 0;
        let leader_slots = HashMap::from([(u64::MAX, Pubkey::default()), (slot, keypair.pubkey())]);
        let rv = verify_shreds(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv, vec![vec![0]]);
    }

    #[test]
    fn test_sigverify_shreds() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        run_test_sigverify_shreds(&thread_pool, 0xdead_c0de);
    }

    fn run_test_sigverify_shreds_sign(thread_pool: &ThreadPool, slot: Slot) {
        solana_logger::setup();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));

        let num_packets = 32;
        let num_batches = 100;
        let mut packet_batch = PacketBatch::with_capacity(num_packets);
        packet_batch.resize(num_packets, Packet::default());

        for (i, p) in packet_batch.iter_mut().enumerate() {
            let shred = Shred::new_from_data(
                slot,
                0xc0de,
                i as u16,
                &[5; LEGACY_SHRED_DATA_CAPACITY],
                ShredFlags::LAST_SHRED_IN_SLOT,
                1,
                2,
                0xc0de,
            );
            shred.copy_to_packet(p);
        }
        let mut batches = vec![packet_batch; num_batches];
        let keypair = Keypair::new();
        let pubkeys = HashMap::from([(u64::MAX, Pubkey::default()), (slot, keypair.pubkey())]);
        //unsigned
        let rv = verify_shreds(thread_pool, &batches, &pubkeys, &cache);
        assert_eq!(rv, vec![vec![0; num_packets]; num_batches]);
        //signed
        sign_shreds(thread_pool, &keypair, &mut batches);
        let rv = verify_shreds_cpu(thread_pool, &batches, &pubkeys, &cache);
        assert_eq!(rv, vec![vec![1; num_packets]; num_batches]);

        let rv = verify_shreds(thread_pool, &batches, &pubkeys, &cache);
        assert_eq!(rv, vec![vec![1; num_packets]; num_batches]);
    }

    #[test]
    fn test_sigverify_shreds_sign() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        run_test_sigverify_shreds_sign(&thread_pool, 0xdead_c0de);
    }

    fn run_test_sigverify_shreds_sign_cpu(thread_pool: &ThreadPool, slot: Slot) {
        solana_logger::setup();

        let mut batches = [PacketBatch::default()];
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let keypair = Keypair::new();
        let shred = Shred::new_from_data(
            slot,
            0xc0de,
            0xdead,
            &[1, 2, 3, 4],
            ShredFlags::LAST_SHRED_IN_SLOT,
            0,
            0,
            0xc0de,
        );
        batches[0].resize(1, Packet::default());
        batches[0][0].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batches[0][0].meta_mut().size = shred.payload().len();

        let pubkeys = HashMap::from([(slot, keypair.pubkey()), (u64::MAX, Pubkey::default())]);
        //unsigned
        let rv = verify_shreds_cpu(thread_pool, &batches, &pubkeys, &cache);
        assert_eq!(rv, vec![vec![0]]);
        //signed
        sign_shreds_cpu(thread_pool, &keypair, &mut batches);
        let rv = verify_shreds_cpu(thread_pool, &batches, &pubkeys, &cache);
        assert_eq!(rv, vec![vec![1]]);
    }

    #[test]
    fn test_sigverify_shreds_sign_cpu() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        run_test_sigverify_shreds_sign_cpu(&thread_pool, 0xdead_c0de);
    }

    fn make_transaction<R: Rng>(rng: &mut R) -> Transaction {
        let block = rng.gen::<[u8; 32]>();
        let recent_blockhash = hash::hashv(&[&block]);
        system_transaction::transfer(
            &Keypair::new(),       // from
            &Pubkey::new_unique(), // to
            rng.gen(),             // lamports
            recent_blockhash,
        )
    }

    fn make_entry<R: Rng>(rng: &mut R, prev_hash: &Hash) -> Entry {
        let size = rng.gen_range(16..32);
        let txs = repeat_with(|| make_transaction(rng)).take(size).collect();
        Entry::new(
            prev_hash,
            rng.gen_range(1..64), // num_hashes
            txs,
        )
    }

    fn make_entries<R: Rng>(rng: &mut R, num_entries: usize) -> Vec<Entry> {
        let prev_hash = hash::hashv(&[&rng.gen::<[u8; 32]>()]);
        let entry = make_entry(rng, &prev_hash);
        std::iter::successors(Some(entry), |entry| Some(make_entry(rng, &entry.hash)))
            .take(num_entries)
            .collect()
    }

    fn make_shreds<R: Rng>(
        rng: &mut R,
        chained: bool,
        is_last_in_slot: bool,
        keypairs: &HashMap<Slot, Keypair>,
    ) -> Vec<Shred> {
        let reed_solomon_cache = ReedSolomonCache::default();
        let mut shreds: Vec<_> = keypairs
            .iter()
            .flat_map(|(&slot, keypair)| {
                let parent_slot = slot - rng.gen::<u16>().max(1) as Slot;
                let num_entries = rng.gen_range(64..128);
                let (data_shreds, coding_shreds) = Shredder::new(
                    slot,
                    parent_slot,
                    rng.gen_range(0..0x40), // reference_tick
                    rng.gen(),              // version
                )
                .unwrap()
                .entries_to_shreds(
                    keypair,
                    &make_entries(rng, num_entries),
                    is_last_in_slot,
                    // chained_merkle_root
                    chained.then(|| Hash::new_from_array(rng.gen())),
                    rng.gen_range(0..2671), // next_shred_index
                    rng.gen_range(0..2781), // next_code_index
                    rng.gen(),              // merkle_variant,
                    &reed_solomon_cache,
                    &mut ProcessShredsStats::default(),
                );
                [data_shreds, coding_shreds]
            })
            .flatten()
            .collect();
        shreds.shuffle(rng);
        // Assert that all shreds verfiy and sanitize.
        for shred in &shreds {
            let pubkey = keypairs[&shred.slot()].pubkey();
            assert!(shred.verify(&pubkey));
            assert_matches!(shred.sanitize(), Ok(()));
        }
        // Verfiy using layout api.
        for shred in &shreds {
            let shred = shred.payload();
            let slot = shred::layout::get_slot(shred).unwrap();
            let signature = shred::layout::get_signature(shred).unwrap();
            let pubkey = keypairs[&slot].pubkey();
            if let Some(offsets) = shred::layout::get_signed_data_offsets(shred) {
                assert!(signature.verify(pubkey.as_ref(), &shred[offsets]));
            }
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
        let packets: Vec<_> = repeat_with(|| {
            let size = rng.gen_range(0..16);
            let packets = packets.by_ref().take(size).collect();
            let batch = PacketBatch::new(packets);
            (size == 0 || !batch.is_empty()).then_some(batch)
        })
        .while_some()
        .collect();
        assert_eq!(
            shreds.len(),
            packets.iter().map(PacketBatch::len).sum::<usize>()
        );
        packets
    }

    #[test_case(false, false)]
    #[test_case(false, true)]
    #[test_case(true, false)]
    #[test_case(true, true)]
    fn test_verify_shreds_fuzz(chained: bool, is_last_in_slot: bool) {
        let mut rng = rand::thread_rng();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let keypairs = repeat_with(|| rng.gen_range(169_367_809..169_906_789))
            .map(|slot| (slot, Keypair::new()))
            .take(3)
            .collect();
        let shreds = make_shreds(&mut rng, chained, is_last_in_slot, &keypairs);
        let pubkeys: HashMap<Slot, Pubkey> = keypairs
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
                packets
                    .iter_mut()
                    .map(|packet| {
                        let coin_flip: bool = rng.gen();
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

    #[test_case(false, false)]
    #[test_case(false, true)]
    #[test_case(true, false)]
    #[test_case(true, true)]
    fn test_sign_shreds_gpu(chained: bool, is_last_in_slot: bool) {
        let mut rng = rand::thread_rng();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let shreds = {
            let keypairs = repeat_with(|| rng.gen_range(169_367_809..169_906_789))
                .map(|slot| (slot, Keypair::new()))
                .take(3)
                .collect();
            make_shreds(&mut rng, chained, is_last_in_slot, &keypairs)
        };
        let keypair = Keypair::new();
        let pubkeys: HashMap<Slot, Pubkey> = {
            let pubkey = keypair.pubkey();
            shreds
                .iter()
                .map(Shred::slot)
                .map(|slot| (slot, pubkey))
                .chain(once((Slot::MAX, Pubkey::default())))
                .collect()
        };
        let mut packets = make_packets(&mut rng, &shreds);
        // Assert that initially all signatrues are invalid.
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
