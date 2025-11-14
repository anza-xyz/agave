#![allow(clippy::implicit_hasher)]
use {
    crate::shred::{self, SIZE_OF_MERKLE_ROOT},
    itertools::{izip, Itertools},
    rayon::{prelude::*, ThreadPool},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_metrics::inc_new_counter_debug,
    solana_nohash_hasher::BuildNoHashHasher,
    solana_perf::{
        cuda_runtime::PinnedVec,
        packet::{BytesPacketBatch, Packet, PacketBatch, PacketRef},
        perf_libs,
        recycler_cache::RecyclerCache,
        sigverify::{self, count_packets_in_batches, TxOffset},
    },
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        borrow::Cow,
        collections::HashMap,
        iter::{self, repeat},
        mem::size_of,
        ops::Range,
        sync::RwLock,
    },
};
#[cfg(test)]
use {
    sha2::{Digest, Sha512},
    solana_keypair::Keypair,
    solana_perf::packet::PacketRefMut,
    solana_signer::Signer,
    std::sync::Arc,
};

#[cfg(test)]
const SIGN_SHRED_GPU_MIN: usize = 256;

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

fn verify_shreds_cpu(
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

fn slot_key_data_for_gpu(
    thread_pool: &ThreadPool,
    batches: &[PacketBatch],
    slot_keys: &SlotPubkeys,
    recycler_cache: &RecyclerCache,
) -> (/*pubkeys:*/ PinnedVec<u8>, TxOffset) {
    //TODO: mark Pubkey::default shreds as failed after the GPU returns
    assert_eq!(slot_keys.get(&Slot::MAX), Some(&Pubkey::default()));
    let slots: Vec<Slot> = thread_pool.install(|| {
        batches
            .into_par_iter()
            .flat_map_iter(|batch| {
                batch.iter().map(|packet| {
                    if packet.meta().discard() {
                        return Slot::MAX;
                    }
                    let shred = shred::layout::get_shred(packet);
                    match shred.and_then(shred::layout::get_slot) {
                        Some(slot) if slot_keys.contains_key(&slot) => slot,
                        _ => Slot::MAX,
                    }
                })
            })
            .collect()
    });
    let keys_to_slots: HashMap<Pubkey, Vec<Slot>> = slots
        .iter()
        .map(|slot| (slot_keys[slot], *slot))
        .into_group_map();
    let mut keyvec = recycler_cache.buffer().allocate("shred_gpu_pubkeys");
    keyvec.set_pinnable();

    let keyvec_size = keys_to_slots.len() * size_of::<Pubkey>();
    resize_buffer(&mut keyvec, keyvec_size);

    let key_offsets: HashMap<Slot, /*key offset:*/ usize> = {
        let mut next_offset = 0;
        keys_to_slots
            .into_iter()
            .flat_map(|(key, slots)| {
                let offset = next_offset;
                next_offset += std::mem::size_of::<Pubkey>();
                keyvec[offset..next_offset].copy_from_slice(key.as_ref());
                slots.into_iter().zip(repeat(offset))
            })
            .collect()
    };
    let mut offsets = recycler_cache.offsets().allocate("shred_offsets");
    offsets.set_pinnable();
    for slot in slots {
        offsets.push(key_offsets[&slot] as u32);
    }
    trace!("keyvec.len: {}", keyvec.len());
    trace!("keyvec: {keyvec:?}");
    trace!("offsets: {offsets:?}");
    (keyvec, offsets)
}

// Recovers merkle roots from shreds binary.
fn get_merkle_roots(
    thread_pool: &ThreadPool,
    packets: &[PacketBatch],
    recycler_cache: &RecyclerCache,
) -> (
    PinnedVec<u8>,      // Merkle roots
    Vec<Option<usize>>, // Offsets
) {
    let merkle_roots: Vec<Option<Hash>> = thread_pool.install(|| {
        packets
            .par_iter()
            .flat_map(|packets| {
                packets.par_iter().map(|packet| {
                    if packet.meta().discard() {
                        return None;
                    }
                    let shred = shred::layout::get_shred(packet)?;
                    shred::layout::get_merkle_root(shred)
                })
            })
            .collect()
    });
    let num_merkle_roots = merkle_roots.iter().flatten().count();
    let mut buffer = recycler_cache.buffer().allocate("shred_gpu_merkle_roots");
    buffer.set_pinnable();
    resize_buffer(&mut buffer, num_merkle_roots * SIZE_OF_MERKLE_ROOT);
    let offsets = {
        let mut next_offset = 0;
        merkle_roots
            .into_iter()
            .map(|root| {
                let root = root?;
                let offset = next_offset;
                next_offset += SIZE_OF_MERKLE_ROOT;
                buffer[offset..next_offset].copy_from_slice(root.as_ref());
                Some(offset)
            })
            .collect()
    };
    (buffer, offsets)
}

// Resizes the buffer to >= size and a multiple of
// std::mem::size_of::<Packet>().
fn resize_buffer(buffer: &mut PinnedVec<u8>, size: usize) {
    //HACK: Pubkeys vector is passed along as a `PacketBatch` buffer to the GPU
    //TODO: GPU needs a more opaque interface, which can handle variable sized structures for data
    //Pad the Pubkeys buffer such that it is bigger than a buffer of Packet sized elems
    let num_packets = size.div_ceil(std::mem::size_of::<Packet>());
    let size = num_packets * std::mem::size_of::<Packet>();
    buffer.resize(size, 0u8);
}

fn elems_from_buffer(buffer: &PinnedVec<u8>) -> perf_libs::Elems {
    // resize_buffer ensures that buffer size is a multiple of Packet size.
    debug_assert_eq!(buffer.len() % std::mem::size_of::<Packet>(), 0);
    let num_packets = buffer.len() / std::mem::size_of::<Packet>();
    perf_libs::Elems {
        elems: buffer.as_ptr().cast::<u8>(),
        num: num_packets as u32,
    }
}

// TODO: clean up legacy shred artifacts
fn shred_gpu_offsets(
    offset: usize,
    batches: &[PacketBatch],
    merkle_roots_offsets: impl IntoIterator<Item = Option<usize>>,
    recycler_cache: &RecyclerCache,
) -> (TxOffset, TxOffset, TxOffset) {
    fn add_offset(range: Range<usize>, offset: usize) -> Range<usize> {
        range.start + offset..range.end + offset
    }
    let mut signature_offsets = recycler_cache.offsets().allocate("shred_signatures");
    signature_offsets.set_pinnable();
    let mut msg_start_offsets = recycler_cache.offsets().allocate("shred_msg_starts");
    msg_start_offsets.set_pinnable();
    let mut msg_sizes = recycler_cache.offsets().allocate("shred_msg_sizes");
    msg_sizes.set_pinnable();
    let offsets = std::iter::successors(Some(offset), |offset| {
        offset.checked_add(std::mem::size_of::<Packet>())
    });
    let packets = batches.iter().flatten();
    for (offset, _packet, merkle_root_offset) in izip!(offsets, packets, merkle_roots_offsets) {
        let sig = shred::layout::get_signature_range();
        let sig = add_offset(sig, offset);
        debug_assert_eq!(sig.end - sig.start, std::mem::size_of::<Signature>());
        // Signature may verify for an empty message but the packet will be
        // discarded during deserialization.
        let msg: Range<usize> = match merkle_root_offset {
            None => {
                0..SIZE_OF_MERKLE_ROOT // legacy shreds - remove valid but useless offset
            }
            Some(merkle_root_offset) => {
                merkle_root_offset..merkle_root_offset + SIZE_OF_MERKLE_ROOT
            }
        };
        signature_offsets.push(sig.start as u32);
        msg_start_offsets.push(msg.start as u32);
        let msg_size = msg.end.saturating_sub(msg.start);
        msg_sizes.push(msg_size as u32);
    }
    (signature_offsets, msg_start_offsets, msg_sizes)
}

pub fn verify_shreds_gpu(
    thread_pool: &ThreadPool,
    batches: &[PacketBatch],
    slot_leaders: &SlotPubkeys,
    recycler_cache: &RecyclerCache,
    cache: &RwLock<LruCache>,
) -> Vec<Vec<u8>> {
    let Some(api) = perf_libs::api() else {
        return verify_shreds_cpu(thread_pool, batches, slot_leaders, cache);
    };
    let (pubkeys, pubkey_offsets) =
        slot_key_data_for_gpu(thread_pool, batches, slot_leaders, recycler_cache);
    //HACK: Pubkeys vector is passed along as a `PacketBatch` buffer to the GPU
    //TODO: GPU needs a more opaque interface, which can handle variable sized structures for data
    let (merkle_roots, merkle_roots_offsets) =
        get_merkle_roots(thread_pool, batches, recycler_cache);
    // Merkle roots are placed after pubkeys; adjust offsets accordingly.
    let merkle_roots_offsets = {
        let shift = pubkeys.len();
        merkle_roots_offsets
            .into_iter()
            .map(move |offset| Some(offset? + shift))
    };
    let offset = pubkeys.len() + merkle_roots.len();
    let (signature_offsets, msg_start_offsets, msg_sizes) =
        shred_gpu_offsets(offset, batches, merkle_roots_offsets, recycler_cache);
    let mut out = recycler_cache.buffer().allocate("out_buffer");
    out.set_pinnable();
    out.resize(signature_offsets.len(), 0u8);
    let mut elems = vec![
        elems_from_buffer(&pubkeys),
        elems_from_buffer(&merkle_roots),
    ];
    // `BytesPacketBatch` cannot be directly used in CUDA. We have to retrieve
    // and convert byte batches to pinned batches. We must collect here so that
    // we keep the batches created by `BytesPacketBatch::to_pinned_packet_batch()`
    // alive.
    let pinned_batches = batches
        .iter()
        .map(|batch| match batch {
            PacketBatch::Pinned(batch) => Cow::Borrowed(batch),
            PacketBatch::Bytes(batch) => Cow::Owned(batch.to_pinned_packet_batch()),
            PacketBatch::Single(packet) => {
                // this is ugly, but unused (gpu code) and will be removed shortly in follow up PR
                let mut batch = BytesPacketBatch::with_capacity(1);
                batch.push(packet.clone());
                Cow::Owned(batch.to_pinned_packet_batch())
            }
        })
        .collect::<Vec<_>>();
    elems.extend(pinned_batches.iter().map(|batch| perf_libs::Elems {
        elems: batch.as_ptr().cast::<u8>(),
        num: batch.len() as u32,
    }));
    let num_packets = elems.iter().map(|elem| elem.num).sum();
    trace!("Starting verify num packets: {num_packets}");
    trace!("elem len: {}", elems.len() as u32);
    trace!("packet sizeof: {}", size_of::<Packet>() as u32);
    const USE_NON_DEFAULT_STREAM: u8 = 1;
    unsafe {
        let res = (api.ed25519_verify_many)(
            elems.as_ptr(),
            elems.len() as u32,
            size_of::<Packet>() as u32,
            num_packets,
            signature_offsets.len() as u32,
            msg_sizes.as_ptr(),
            pubkey_offsets.as_ptr(),
            signature_offsets.as_ptr(),
            msg_start_offsets.as_ptr(),
            out.as_mut_ptr(),
            USE_NON_DEFAULT_STREAM,
        );
        if res != 0 {
            trace!("RETURN!!!: {res}");
        }
    }
    trace!("done verify");
    trace!("out buf {out:?}");

    // Each shred has exactly one signature.
    let v_sig_lens = batches
        .iter()
        .map(|batch| iter::repeat_n(1u32, batch.len()));
    let mut rvs: Vec<_> = batches.iter().map(|batch| vec![0u8; batch.len()]).collect();
    sigverify::copy_return_values(v_sig_lens, &out, &mut rvs);

    inc_new_counter_debug!("ed25519_shred_verify_gpu", out.len());
    rvs
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
fn sign_shreds_cpu(thread_pool: &ThreadPool, keypair: &Keypair, batches: &mut [PacketBatch]) {
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
fn sign_shreds_gpu_pinned_keypair(keypair: &Keypair, cache: &RecyclerCache) -> PinnedVec<u8> {
    let mut vec = cache.buffer().allocate("pinned_keypair");
    let pubkey = keypair.pubkey().to_bytes();
    let secret = keypair.secret_bytes();
    let mut hasher = Sha512::default();
    hasher.update(secret);
    let mut result = hasher.finalize();
    result[0] &= 248;
    result[31] &= 63;
    result[31] |= 64;
    let size = pubkey.len() + result.len();
    resize_buffer(&mut vec, size);
    vec[0..pubkey.len()].copy_from_slice(&pubkey);
    vec[pubkey.len()..size].copy_from_slice(&result);
    vec
}

#[cfg(test)]
fn sign_shreds_gpu(
    thread_pool: &ThreadPool,
    keypair: &Keypair,
    pinned_keypair: &Option<Arc<PinnedVec<u8>>>,
    batches: &mut [PacketBatch],
    recycler_cache: &RecyclerCache,
) {
    let sig_size = size_of::<Signature>();
    let pubkey_size = size_of::<Pubkey>();
    let packet_count = count_packets_in_batches(batches);
    if packet_count < SIGN_SHRED_GPU_MIN || pinned_keypair.is_none() {
        return sign_shreds_cpu(thread_pool, keypair, batches);
    }
    let Some(api) = perf_libs::api() else {
        return sign_shreds_cpu(thread_pool, keypair, batches);
    };
    let pinned_keypair = pinned_keypair.as_ref().unwrap();

    //should be zero
    let mut pubkey_offsets = recycler_cache.offsets().allocate("pubkey offsets");
    pubkey_offsets.resize(packet_count, 0);

    let mut secret_offsets = recycler_cache.offsets().allocate("secret_offsets");
    secret_offsets.resize(packet_count, pubkey_size as u32);

    let (merkle_roots, merkle_roots_offsets) =
        get_merkle_roots(thread_pool, batches, recycler_cache);
    // Merkle roots are placed after the keypair; adjust offsets accordingly.
    let merkle_roots_offsets = {
        let shift = pinned_keypair.len();
        merkle_roots_offsets
            .into_iter()
            .map(move |offset| Some(offset? + shift))
    };
    let offset = pinned_keypair.len() + merkle_roots.len();
    trace!("offset: {offset}");
    let (signature_offsets, msg_start_offsets, msg_sizes) =
        shred_gpu_offsets(offset, batches, merkle_roots_offsets, recycler_cache);
    let total_sigs = signature_offsets.len();
    let mut signatures_out = recycler_cache.buffer().allocate("ed25519 signatures");
    signatures_out.set_pinnable();
    signatures_out.resize(total_sigs * sig_size, 0);

    let mut elems = vec![
        elems_from_buffer(pinned_keypair),
        elems_from_buffer(&merkle_roots),
    ];
    // `BytesPacketBatch` cannot be directly used in CUDA. We have to retrieve
    // and convert byte batches to pinned batches. We must collect here so that
    // we keep the batches created by `BytesPacketBatch::to_pinned_packet_batch()`
    // alive.
    let pinned_batches = batches
        .iter_mut()
        .map(|batch| match batch {
            PacketBatch::Pinned(batch) => Cow::Borrowed(batch),
            PacketBatch::Bytes(batch) => Cow::Owned(batch.to_pinned_packet_batch()),
            PacketBatch::Single(packet) => {
                // this is ugly, but unused (gpu code) and will be removed shortly in follow up PR
                let mut batch = BytesPacketBatch::with_capacity(1);
                batch.push(packet.clone());
                Cow::Owned(batch.to_pinned_packet_batch())
            }
        })
        .collect::<Vec<_>>();
    elems.extend(pinned_batches.iter().map(|batch| perf_libs::Elems {
        elems: batch.as_ptr().cast::<u8>(),
        num: batch.len() as u32,
    }));
    let num_packets = elems.iter().map(|elem| elem.num).sum();
    trace!("Starting verify num packets: {num_packets}");
    trace!("elem len: {}", elems.len() as u32);
    trace!("packet sizeof: {}", size_of::<Packet>() as u32);
    const USE_NON_DEFAULT_STREAM: u8 = 1;
    unsafe {
        let res = (api.ed25519_sign_many)(
            elems.as_mut_ptr(),
            elems.len() as u32,
            size_of::<Packet>() as u32,
            num_packets,
            total_sigs as u32,
            msg_sizes.as_ptr(),
            pubkey_offsets.as_ptr(),
            secret_offsets.as_ptr(),
            msg_start_offsets.as_ptr(),
            signatures_out.as_mut_ptr(),
            USE_NON_DEFAULT_STREAM,
        );
        if res != 0 {
            trace!("RETURN!!!: {res}");
        }
    }
    trace!("done sign");
    // Cumulative number of packets within batches.
    let num_packets: Vec<_> = batches
        .iter()
        .scan(0, |num_packets, batch| {
            let out = *num_packets;
            *num_packets += batch.len();
            Some(out)
        })
        .collect();
    thread_pool.install(|| {
        batches
            .par_iter_mut()
            .zip(num_packets)
            .for_each(|(batch, num_packets)| {
                batch
                    .par_iter_mut()
                    .enumerate()
                    .for_each(|(packet_ix, mut packet)| {
                        let sig_ix = packet_ix + num_packets;
                        let sig_start = sig_ix * sig_size;
                        let sig_end = sig_start + sig_size;
                        let mut buffer = packet
                            .data(..)
                            .expect("expected the packet to not be discarded")
                            .to_vec();
                        buffer[..sig_size].copy_from_slice(&signatures_out[sig_start..sig_end]);
                        packet.copy_from_slice(&buffer);
                    });
            });
    });
    inc_new_counter_debug!("ed25519_shred_sign_gpu", packet_count);
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
        rand::{seq::SliceRandom, Rng},
        rayon::ThreadPoolBuilder,
        solana_entry::entry::Entry,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_perf::packet::PinnedPacketBatch,
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
        let rv = verify_shreds_cpu(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 1);

        let wrong_keypair = Keypair::new();
        let leader_slots: SlotPubkeys = [(slot, wrong_keypair.pubkey())].into_iter().collect();
        let rv = verify_shreds_cpu(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);

        let leader_slots: SlotPubkeys = HashMap::default();
        let rv = verify_shreds_cpu(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);

        let leader_slots: SlotPubkeys = [(slot, keypair.pubkey())].into_iter().collect();
        batches[0]
            .iter_mut()
            .for_each(|mut packet_ref| packet_ref.meta_mut().size = 0);
        let rv = verify_shreds_cpu(thread_pool, &batches, &leader_slots, &cache);
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);
    }

    #[test]
    fn test_sigverify_shreds_cpu() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        run_test_sigverify_shreds_cpu(&thread_pool, 0xdead_c0de);
    }

    fn run_test_sigverify_shreds_gpu(thread_pool: &ThreadPool, slot: Slot) {
        agave_logger::setup();
        let recycler_cache = RecyclerCache::default();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));

        let keypair = Keypair::new();
        let batch = make_packet_batch(&keypair, slot);
        let mut batches = [batch];

        let leader_slots: SlotPubkeys = [(u64::MAX, Pubkey::default()), (slot, keypair.pubkey())]
            .into_iter()
            .collect();
        let rv = verify_shreds_gpu(
            thread_pool,
            &batches,
            &leader_slots,
            &recycler_cache,
            &cache,
        );
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 1);

        let wrong_keypair = Keypair::new();
        let leader_slots: SlotPubkeys = [
            (u64::MAX, Pubkey::default()),
            (slot, wrong_keypair.pubkey()),
        ]
        .into_iter()
        .collect();
        let rv = verify_shreds_gpu(
            thread_pool,
            &batches,
            &leader_slots,
            &recycler_cache,
            &cache,
        );
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);

        let leader_slots: SlotPubkeys = [(u64::MAX, Pubkey::default())].into_iter().collect();
        let rv = verify_shreds_gpu(
            thread_pool,
            &batches,
            &leader_slots,
            &recycler_cache,
            &cache,
        );
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);

        batches[0]
            .iter_mut()
            .for_each(|mut pr| pr.meta_mut().size = 0);
        let leader_slots: SlotPubkeys = [(u64::MAX, Pubkey::default()), (slot, keypair.pubkey())]
            .into_iter()
            .collect();
        let rv = verify_shreds_gpu(
            thread_pool,
            &batches,
            &leader_slots,
            &recycler_cache,
            &cache,
        );
        assert_eq!(rv.into_iter().flatten().all_equal_value().unwrap(), 0);
    }

    fn make_packet_batch(keypair: &Keypair, slot: u64) -> PacketBatch {
        let mut batch = PinnedPacketBatch::default();
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
    fn test_sigverify_shreds_gpu() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        run_test_sigverify_shreds_gpu(&thread_pool, 0xdead_c0de);
    }

    fn make_transaction<R: Rng>(rng: &mut R) -> Transaction {
        let block = rng.gen::<[u8; 32]>();
        let recent_blockhash = solana_sha256_hasher::hashv(&[&block]);
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
        let prev_hash = solana_sha256_hasher::hashv(&[&rng.gen::<[u8; 32]>()]);
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
                let parent_slot = slot - rng.gen::<u16>().max(1) as Slot;
                let num_entries = rng.gen_range(64..128);
                Shredder::new(
                    slot,
                    parent_slot,
                    rng.gen_range(0..0x40), // reference_tick
                    rng.gen(),              // version
                )
                .unwrap()
                .make_merkle_shreds_from_entries(
                    keypair,
                    &make_entries(rng, num_entries),
                    is_last_in_slot,
                    Hash::new_from_array(rng.gen()), // chained_merkle_root
                    rng.gen_range(0..2671),          // next_shred_index
                    rng.gen_range(0..2781),          // next_code_index
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
            let size = rng.gen_range(0..16);
            let packets = packets.by_ref().take(size).collect();
            let batch = PinnedPacketBatch::new(packets);
            (size == 0 || !batch.is_empty()).then_some(batch.into())
        })
        .while_some()
        .collect();
        assert_eq!(
            shreds.len(),
            packets.iter().map(|batch| batch.len()).sum::<usize>()
        );
        assert!(count_packets_in_batches(&packets) > SIGN_SHRED_GPU_MIN);
        packets
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_verify_shreds_fuzz(is_last_in_slot: bool) {
        let mut rng = rand::thread_rng();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let recycler_cache = RecyclerCache::default();
        let keypairs = repeat_with(|| rng.gen_range(169_367_809..169_906_789))
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
            verify_shreds_gpu(&thread_pool, &packets, &pubkeys, &recycler_cache, &cache),
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
                        let coin_flip: bool = rng.gen();
                        if !coin_flip {
                            shred::layout::corrupt_packet(&mut rng, packet, &keypairs);
                        }
                        u8::from(coin_flip)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            verify_shreds_gpu(&thread_pool, &packets, &pubkeys, &recycler_cache, &cache),
            out
        );
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_sign_shreds_gpu(is_last_in_slot: bool) {
        let mut rng = rand::thread_rng();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let recycler_cache = RecyclerCache::default();
        let shreds = {
            let keypairs = repeat_with(|| rng.gen_range(169_367_809..169_906_789))
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
        // Assert that initially all signatrues are invalid.
        assert_eq!(
            verify_shreds_gpu(&thread_pool, &packets, &pubkeys, &recycler_cache, &cache),
            packets
                .iter()
                .map(|batch| vec![0u8; batch.len()])
                .collect::<Vec<_>>()
        );
        let pinned_keypair = sign_shreds_gpu_pinned_keypair(&keypair, &recycler_cache);
        let pinned_keypair = Some(Arc::new(pinned_keypair));
        // Sign and verify shreds signatures.
        sign_shreds_gpu(
            &thread_pool,
            &keypair,
            &pinned_keypair,
            &mut packets,
            &recycler_cache,
        );
        assert_eq!(
            verify_shreds_gpu(&thread_pool, &packets, &pubkeys, &recycler_cache, &cache),
            packets
                .iter()
                .map(|batch| vec![1u8; batch.len()])
                .collect::<Vec<_>>()
        );
    }
}
