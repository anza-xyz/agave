//! Service to send transaction packets to the external scheduler.
//!

use {
    agave_banking_stage_ingress_types::BankingPacketReceiver,
    agave_scheduler_bindings::{tpu_message_flags, SharableTransactionRegion, TpuToPackMessage},
    rts_alloc::Allocator,
    solana_packet::PacketFlags,
    solana_perf::packet::PacketBatch,
    std::{
        net::IpAddr,
        path::{Path, PathBuf},
        ptr::NonNull,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::JoinHandle,
    },
};

/// Spawns a thread to receive packets from TPU and send them to the external scheduler.
///
/// # Safety:
/// - `allocator_worker_id` must be unique among all processes using the same allocator path.
pub unsafe fn spawn(
    exit: Arc<AtomicBool>,
    non_vote_receiver: BankingPacketReceiver,
    vote_receivers: Option<(BankingPacketReceiver, BankingPacketReceiver)>,
    allocator_path: PathBuf,
    allocator_worker_id: u32,
    queue_path: PathBuf,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("solTpu2Pack".to_string())
        .spawn(move || {
            // Setup allocator and queue
            // SAFETY: The caller must ensure that no other process is using the same worker id.
            if let Some((allocator, producer)) =
                unsafe { setup(allocator_path, allocator_worker_id, queue_path) }
            {
                tpu_to_pack(exit, non_vote_receiver, vote_receivers, allocator, producer);
            }
        })
        .unwrap()
}

fn tpu_to_pack(
    exit: Arc<AtomicBool>,
    non_vote_receiver: BankingPacketReceiver,
    vote_receivers: Option<(BankingPacketReceiver, BankingPacketReceiver)>,
    allocator: Allocator,
    mut producer: shaq::Producer<TpuToPackMessage>,
) {
    // Create a round-robin receiver for vote/non-vote packets.
    // If vote receivers are not provided, this just always returns the non-vote receiver.
    let mut round_robin_receiver_iter = if let Some(vote_receivers) = vote_receivers {
        vec![vote_receivers.0, vote_receivers.1, non_vote_receiver]
            .into_iter()
            .cycle()
    } else {
        vec![non_vote_receiver].into_iter().cycle()
    };

    while !exit.load(Ordering::Relaxed) {
        // Receive packets from the next receiver in round-robin order.
        if let Ok(packet_batches) = round_robin_receiver_iter.next().unwrap().try_recv() {
            handle_packet_batches(&allocator, &mut producer, packet_batches);
        };
    }
}

fn handle_packet_batches(
    allocator: &Allocator,
    producer: &mut shaq::Producer<TpuToPackMessage>,
    packet_batches: Arc<Vec<PacketBatch>>,
) {
    // Clean all remote frees in allocator so we have as much
    // room as possible.
    allocator.clean_remote_free_lists();

    // Sync producer queue with reader so we have as much room as possible.
    producer.sync();

    'batch_loop: for batch in packet_batches.iter() {
        for packet in batch.iter() {
            // Check if the packet is valid and get the bytes.
            let Some(packet_bytes) = packet.data(..) else {
                continue;
            };
            let packet_size = packet_bytes.len();

            // Allocate enough memory for the packet in the allocator.
            let Some(allocated_ptr) = allocator.allocate(packet_size as u32) else {
                // Failed to allocate memory for the packet, drop the rest of the batch.
                warn!("Failed to allocate memory for packet. Dropping the rest of the batch.");
                break 'batch_loop;
            };

            // Reserve space in the producer queue for the packet message.
            let Some(tpu_to_pack_message) = producer.reserve() else {
                // Free the allocated packet if we can't reserve space in the queue.
                // SAFETY: `allocated_ptr` was allocated from `allocator`.
                unsafe {
                    allocator.free(allocated_ptr);
                }
                break 'batch_loop;
            };

            // Copy the packet data into the allocated memory.
            // SAFETY:
            // - `allocated_ptr` is valid for `packet_size` bytes.
            // - src and dst are valid pointers that are properly aligned
            //   and do not overlap.
            unsafe {
                allocated_ptr.copy_from_nonoverlapping(
                    NonNull::new(packet_bytes.as_ptr().cast_mut())
                        .expect("packet bytes must be non-null"),
                    packet_size,
                );
            }

            // Create a sharable transaction region for the packet.
            let transaction = SharableTransactionRegion {
                // SAFETY: `allocated_ptr` was allocated from `allocator`.
                offset: unsafe { allocator.offset(allocated_ptr) },
                length: packet_size as u32,
            };

            // Translate flags from meta.
            let tpu_message_flags = flags_from_meta(packet.meta().flags);

            // Get the source address of the packet - convert to expected format.
            let src_addr = map_src_addr(packet.meta().addr);

            // Populate the message and write it to the queue.
            unsafe {
                tpu_to_pack_message.write(TpuToPackMessage {
                    transaction,
                    flags: tpu_message_flags,
                    src_addr,
                });
            }
        }
    }

    // Commit the messages to the producer queue.
    // This makes the messages available to the consumer.
    producer.commit();
}

fn flags_from_meta(flags: PacketFlags) -> u8 {
    let mut tpu_message_flags = 0;

    if flags.contains(PacketFlags::SIMPLE_VOTE_TX) {
        tpu_message_flags |= tpu_message_flags::IS_SIMPLE_VOTE;
    }
    if flags.contains(PacketFlags::FORWARDED) {
        tpu_message_flags |= tpu_message_flags::FORWARDED;
    }
    if flags.contains(PacketFlags::FROM_STAKED_NODE) {
        tpu_message_flags |= tpu_message_flags::FROM_STAKED_NODE;
    }

    tpu_message_flags
}

fn map_src_addr(addr: IpAddr) -> [u8; 16] {
    match addr {
        IpAddr::V4(ipv4) => ipv4.to_ipv6_mapped().octets(),
        IpAddr::V6(ipv6) => ipv6.octets(),
    }
}

/// # Safety:
/// - `allocator_worker_id` must be unique among all processes using the same allocator path.
unsafe fn setup(
    allocator_path: impl AsRef<Path>,
    allocator_worker_id: u32,
    queue_path: impl AsRef<Path>,
) -> Option<(Allocator, shaq::Producer<TpuToPackMessage>)> {
    // SAFETY: The caller must ensure that no other process is using the same worker id.
    let allocator = unsafe { Allocator::join(allocator_path, allocator_worker_id) }
        .map_err(|err| {
            error!("Failed to join allocator: {err:?}");
        })
        .ok()?;

    let producer = shaq::Producer::join(queue_path)
        .map_err(|err| {
            error!("Failed to join queue: {err:?}");
        })
        .ok()?;

    Some((allocator, producer))
}
