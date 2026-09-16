#![allow(clippy::implicit_hasher)]
use {
    crate::shred,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_nohash_hasher::BuildNoHashHasher,
    solana_perf::packet::PacketRef,
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
    let Some(data) = shred::layout::get_merkle_root(shred) else {
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

#[cfg(test)]
fn sign_shred_cpu(keypair: &Keypair, packet: &mut PacketRefMut) {
    let sig = shred::layout::SIGNATURE_RANGE;
    let msg = shred::layout::get_shred(packet.as_ref())
        .and_then(shred::layout::get_merkle_root)
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
mod tests {
    use {
        super::*,
        crate::{
            shred::{ProcessShredsStats, Shred},
            shredder::{ReedSolomonCache, Shredder},
        },
        assert_matches::assert_matches,
        itertools::Itertools,
        rand::{Rng, seq::SliceRandom},
        solana_entry::entry::Entry,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_packet::Packet,
        solana_perf::packet::{PacketBatch, RecycledPacketBatch},
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        solana_transaction::Transaction,
        std::iter::{once, repeat_with},
        test_case::test_case,
    };

    fn sign_shreds(keypair: &Keypair, batches: &mut [PacketBatch]) {
        for batch in batches {
            for mut packet in batch.iter_mut() {
                sign_shred_cpu(keypair, &mut packet);
            }
        }
    }

    fn verify_batches(
        batches: &mut [PacketBatch],
        slot_leaders: &SlotPubkeys,
        cache: &RwLock<LruCache>,
    ) {
        for batch in batches {
            for mut packet in batch.iter_mut() {
                if !packet.meta().discard()
                    && !verify_shred_cpu(packet.as_ref(), slot_leaders, cache)
                {
                    packet.meta_mut().set_discard(true);
                }
            }
        }
    }

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
                .into_iter()
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
            let data = shred::layout::get_merkle_root(shred).unwrap();
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

        verify_batches(&mut packets, &pubkeys, &cache);
        assert!(
            packets
                .iter()
                .flatten()
                .all(|packet| !packet.meta().discard())
        );

        // Invalidate signatures for a random number of packets.
        let expected_discards = packets
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
                        !coin_flip
                    })
                    .collect::<Vec<bool>>()
            })
            .collect::<Vec<_>>();

        verify_batches(&mut packets, &pubkeys, &cache);
        assert!(
            packets
                .iter()
                .zip(expected_discards.iter())
                .all(|(batch, expected)| {
                    batch
                        .iter()
                        .zip(expected)
                        .all(|(packet, should_discard)| packet.meta().discard() == *should_discard)
                })
        );
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_sign_shreds(is_last_in_slot: bool) {
        let mut rng = rand::rng();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
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
        verify_batches(&mut packets, &pubkeys, &cache);
        assert!(
            packets
                .iter()
                .flatten()
                .all(|packet| packet.meta().discard())
        );

        packets.iter_mut().for_each(|batch| {
            batch
                .iter_mut()
                .for_each(|mut packet| packet.meta_mut().set_discard(false));
        });

        // Sign and verify shred signatures.
        sign_shreds(&keypair, &mut packets);
        verify_batches(&mut packets, &pubkeys, &cache);
        assert!(
            packets
                .iter()
                .flatten()
                .all(|packet| !packet.meta().discard())
        );
    }
}
