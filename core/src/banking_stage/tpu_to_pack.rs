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

pub struct BankingPacketReceivers {
    pub non_vote_receiver: BankingPacketReceiver,
    pub gossip_vote_receiver: Option<BankingPacketReceiver>,
    pub tpu_vote_receiver: Option<BankingPacketReceiver>,
}

/// Spawns a thread to receive packets from TPU and send them to the external scheduler.
///
/// # Safety:
/// - `allocator_worker_id` must be unique among all processes using the same allocator path.
pub unsafe fn spawn(
    exit: Arc<AtomicBool>,
    receivers: BankingPacketReceivers,
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
                tpu_to_pack(exit, receivers, allocator, producer);
            }
        })
        .unwrap()
}

fn tpu_to_pack(
    exit: Arc<AtomicBool>,
    receivers: BankingPacketReceivers,
    allocator: Allocator,
    mut producer: shaq::Producer<TpuToPackMessage>,
) {
    // select! requires actual receivers, so in the case of None for vote receivers,
    // we create a dummy channel that can never receive.
    let non_vote_receiver = receivers.non_vote_receiver;
    let gossip_vote_receiver = receivers
        .gossip_vote_receiver
        .unwrap_or_else(crossbeam_channel::never);
    let tpu_vote_receiver = receivers
        .tpu_vote_receiver
        .unwrap_or_else(crossbeam_channel::never);

    while !exit.load(Ordering::Relaxed) {
        let packet_batches = match crossbeam_channel::select! {
            recv(non_vote_receiver) -> msg => msg,
            recv(gossip_vote_receiver) -> msg => msg,
            recv(tpu_vote_receiver) -> msg => msg,
        } {
            Ok(packet_batches) => packet_batches,
            Err(crossbeam_channel::RecvError) => {
                // Senders have been dropped, exit the loop.
                break;
            }
        };
        handle_packet_batches(&allocator, &mut producer, packet_batches);
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

            let Some((allocated_ptr, tpu_to_pack_message)) =
                allocate_and_reserve_message(allocator, producer, packet_size)
            else {
                warn!("Failed to allocate/reserve message. Dropping the rest of the batch.");
                break 'batch_loop;
            };

            // Get the offset of the allocated pointer in the allocator.
            // SAFETY: `allocated_ptr` was allocated from `allocator`.
            let allocated_ptr_offset_in_allocator = unsafe { allocator.offset(allocated_ptr) };

            // SAFETY:
            // - `allocated_ptr` is valid for `packet_size` bytes.
            // - `tpu_to_pack_message` is a valid pointer to a `TpuToPackMessage`.
            unsafe {
                copy_packet_and_populate_message(
                    packet_bytes,
                    packet.meta(),
                    allocated_ptr,
                    allocated_ptr_offset_in_allocator,
                    tpu_to_pack_message,
                );
            }
        }
    }

    // Commit the messages to the producer queue.
    // This makes the messages available to the consumer.
    producer.commit();
}

fn allocate_and_reserve_message(
    allocator: &Allocator,
    producer: &mut shaq::Producer<TpuToPackMessage>,
    packet_size: usize,
) -> Option<(NonNull<u8>, NonNull<TpuToPackMessage>)> {
    // Allocate enough memory for the packet in the allocator.
    let allocated_ptr = allocator.allocate(packet_size as u32)?;

    // Reserve space in the producer queue for the packet message.
    let Some(tpu_to_pack_message) = producer.reserve() else {
        // Free the allocated packet if we can't reserve space in the queue.
        // SAFETY: `allocated_ptr` was allocated from `allocator`.
        unsafe {
            allocator.free(allocated_ptr);
        }
        return None;
    };

    Some((allocated_ptr, tpu_to_pack_message))
}

/// # Safety:
/// - `allocated_ptr` must be valid for `packet_bytes.len()` bytes.
/// - `tpu_to_pack_message` must be a valid pointer to a `TpuToPackMessage`.
unsafe fn copy_packet_and_populate_message(
    packet_bytes: &[u8],
    packet_meta: &solana_packet::Meta,
    allocated_ptr: NonNull<u8>,
    allocated_ptr_offset_in_allocator: usize,
    tpu_to_pack_message: NonNull<TpuToPackMessage>,
) {
    // Copy the packet data into the allocated memory.
    // SAFETY:
    // - `allocated_ptr` is valid for `packet_size` bytes.
    // - src and dst are valid pointers that are properly aligned
    //   and do not overlap.
    unsafe {
        allocated_ptr.copy_from_nonoverlapping(
            NonNull::new(packet_bytes.as_ptr().cast_mut()).expect("packet bytes must be non-null"),
            packet_bytes.len(),
        );
    }

    // Create a sharable transaction region for the packet.
    let transaction = SharableTransactionRegion {
        offset: allocated_ptr_offset_in_allocator,
        length: packet_bytes.len() as u32,
    };

    // Translate flags from meta.
    let tpu_message_flags = flags_from_meta(packet_meta.flags);

    // Get the source address of the packet - convert to expected format.
    let src_addr = map_src_addr(packet_meta.addr);

    // Populate the message and write it to the queue.
    // SAFETY: `tpu_to_pack_message` is a valid pointer to a `TpuToPackMessage`.
    unsafe {
        tpu_to_pack_message.write(TpuToPackMessage {
            transaction,
            flags: tpu_message_flags,
            src_addr,
        });
    }
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

#[cfg(test)]
mod tests {
    use {super::*, std::net::Ipv4Addr};

    #[test]
    fn test_copy_packet_and_populate_message() {
        let packet_bytes = vec![1, 2, 3, 4, 5];
        let src_ip = Ipv4Addr::new(192, 168, 1, 1);
        let packet_meta = solana_packet::Meta {
            size: packet_bytes.len(),
            addr: IpAddr::V4(src_ip),
            port: 1,
            flags: PacketFlags::all(),
        };

        // Buffer to simulate allocated memory
        let mut buffer = [0u8; 256];
        let mut tpu_to_pack_message = TpuToPackMessage {
            transaction: SharableTransactionRegion {
                offset: 0,
                length: 0,
            },
            flags: 0,
            src_addr: [0; 16],
        };
        const DUMMY_OFFSET: usize = 42;

        unsafe {
            copy_packet_and_populate_message(
                packet_bytes.as_slice(),
                &packet_meta,
                NonNull::new(buffer.as_mut_ptr()).unwrap(),
                DUMMY_OFFSET,
                NonNull::new(&mut tpu_to_pack_message as *mut TpuToPackMessage).unwrap(),
            );
        }

        assert_eq!(&buffer[..packet_bytes.len()], packet_bytes.as_slice());
        assert_eq!(tpu_to_pack_message.transaction.offset, DUMMY_OFFSET);
        assert_eq!(
            tpu_to_pack_message.transaction.length,
            packet_bytes.len() as u32
        );
        assert_eq!(
            tpu_to_pack_message.flags,
            tpu_message_flags::IS_SIMPLE_VOTE
                | tpu_message_flags::FORWARDED
                | tpu_message_flags::FROM_STAKED_NODE
        );
        assert_eq!(
            tpu_to_pack_message.src_addr,
            src_ip.to_ipv6_mapped().octets()
        );
    }

    #[test]
    fn test_flags_from_meta() {
        assert_eq!(
            flags_from_meta(PacketFlags::empty()),
            tpu_message_flags::NONE
        );
        assert_eq!(
            flags_from_meta(PacketFlags::SIMPLE_VOTE_TX),
            tpu_message_flags::IS_SIMPLE_VOTE
        );
        assert_eq!(
            flags_from_meta(PacketFlags::FORWARDED),
            tpu_message_flags::FORWARDED
        );
        assert_eq!(
            flags_from_meta(PacketFlags::FROM_STAKED_NODE),
            tpu_message_flags::FROM_STAKED_NODE
        );
        assert_eq!(
            flags_from_meta(
                PacketFlags::SIMPLE_VOTE_TX
                    | PacketFlags::FORWARDED
                    | PacketFlags::FROM_STAKED_NODE
            ),
            tpu_message_flags::IS_SIMPLE_VOTE
                | tpu_message_flags::FORWARDED
                | tpu_message_flags::FROM_STAKED_NODE
        );
    }
}
