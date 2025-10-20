//! The `shred_fetch_stage` pulls shreds from UDP sockets and sends it to a channel.

use {
    crate::repair::{repair_service::OutstandingShredRepairs, serve_repair::ServeRepair},
    agave_feature_set::FeatureSet,
    bytes::Bytes,
    crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender},
    itertools::Itertools,
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::shred::{self, should_discard_shred, ShredFetchStats},
    solana_packet::{Meta, PACKET_DATA_SIZE},
    solana_perf::packet::{
        BytesPacket, BytesPacketBatch, PacketBatch, PacketBatchRecycler, PacketFlags, PacketRef,
    },
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::BankForks,
    solana_streamer::{
        evicting_sender::EvictingSender,
        streamer::{self, ChannelSend, PacketBatchReceiver, StreamerReceiveStats},
    },
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

// When running with very short epochs (e.g. for testing), we want to avoid
// filtering out shreds that we actually need. This value was chosen empirically
// because it's large enough to protect against observed short epoch problems
// while being small enough to keep the overhead small on deduper, blockstore,
// etc.
const MAX_SHRED_DISTANCE_MINIMUM: u64 = 500;

pub(crate) struct ShredFetchStage {
    thread_hdls: Vec<JoinHandle<()>>,
}

/// Ingress limit for the shred fetch channel (in terms of packet _batches_).
///
/// The general case sees shred and repair ingress in the hundreds of packet batches per second.
/// However, in the case of catch-up, we may see upwards of 8k packet batches per second, which would
/// suggest a roughly 16k packet batch limit for ample headroom. We're setting it to 4x that amount
/// to future proof for increases of CU limits (e.g., a future 100k CU limit).
pub(crate) const SHRED_FETCH_CHANNEL_SIZE: usize = 1024 * 64;

#[derive(Clone)]
struct RepairContext {
    repair_socket: Arc<UdpSocket>,
    cluster_info: Arc<ClusterInfo>,
    outstanding_repair_requests: Arc<RwLock<OutstandingShredRepairs>>,
}

impl ShredFetchStage {
    // updates packets received on a channel and sends them on another channel
    fn modify_packets(
        recvr: PacketBatchReceiver,
        recvr_stats: Option<Arc<StreamerReceiveStats>>,
        sendr: EvictingSender<PacketBatch>,
        bank_forks: &RwLock<BankForks>,
        shred_version: u16,
        name: &'static str,
        flags: PacketFlags,
        repair_context: Option<&RepairContext>,
        turbine_disabled: Arc<AtomicBool>,
    ) {
        // Only repair shreds need repair context.
        debug_assert_eq!(
            flags.contains(PacketFlags::REPAIR),
            repair_context.is_some()
        );
        const STATS_SUBMIT_CADENCE: Duration = Duration::from_secs(1);
        let mut last_updated = Instant::now();
        let mut keypair = repair_context.as_ref().copied().map(RepairContext::keypair);
        let (
            mut last_root,
            mut slots_per_epoch,
            mut _feature_set,
            mut _epoch_schedule,
            mut last_slot,
        ) = {
            let bank_forks_r = bank_forks.read().unwrap();
            let root_bank = bank_forks_r.root_bank();
            (
                root_bank.slot(),
                root_bank.get_slots_in_epoch(root_bank.epoch()),
                root_bank.feature_set.clone(),
                root_bank.epoch_schedule().clone(),
                bank_forks_r.highest_slot(),
            )
        };
        let mut stats = ShredFetchStats::default();

        for mut packet_batch in recvr {
            if last_updated.elapsed().as_millis() as u64 > DEFAULT_MS_PER_SLOT {
                last_updated = Instant::now();
                let root_bank = {
                    let bank_forks_r = bank_forks.read().unwrap();
                    last_slot = bank_forks_r.highest_slot();
                    bank_forks_r.root_bank()
                };
                _feature_set = root_bank.feature_set.clone();
                _epoch_schedule = root_bank.epoch_schedule().clone();
                last_root = root_bank.slot();
                slots_per_epoch = root_bank.get_slots_in_epoch(root_bank.epoch());
                keypair = repair_context.as_ref().copied().map(RepairContext::keypair);
            }
            stats.shred_count += packet_batch.len();

            if let Some(repair_context) = repair_context {
                debug_assert_eq!(flags, PacketFlags::REPAIR);
                debug_assert!(keypair.is_some());
                if let Some(ref keypair) = keypair {
                    ServeRepair::handle_repair_response_pings(
                        &repair_context.repair_socket,
                        keypair,
                        &mut packet_batch,
                        &mut stats,
                    );
                }
                // Discard packets if repair nonce does not verify.
                let now = solana_time_utils::timestamp();
                let mut outstanding_repair_requests =
                    repair_context.outstanding_repair_requests.write().unwrap();
                packet_batch
                    .iter_mut()
                    .filter(|packet| !packet.meta().discard())
                    .for_each(|mut packet| {
                        // Have to set repair flag here so that the nonce is
                        // taken off the shred's payload.
                        packet.meta_mut().flags |= PacketFlags::REPAIR;
                        if !verify_repair_nonce(
                            packet.as_ref(),
                            now,
                            &mut outstanding_repair_requests,
                        ) {
                            packet.meta_mut().set_discard(true);
                        }
                    });
            }

            // Filter out shreds that are way too far in the future to avoid the
            // overhead of having to hold onto them.
            let max_slot = last_slot + MAX_SHRED_DISTANCE_MINIMUM.max(2 * slots_per_epoch);
            let turbine_disabled = turbine_disabled.load(Ordering::Relaxed);
            for mut packet in packet_batch.iter_mut().filter(|p| !p.meta().discard()) {
                if turbine_disabled
                    || should_discard_shred(
                        packet.as_ref(),
                        last_root,
                        max_slot,
                        shred_version,
                        &mut stats,
                    )
                {
                    packet.meta_mut().set_discard(true);
                } else {
                    packet.meta_mut().flags.insert(flags);
                }
            }
            if stats.maybe_submit(name, STATS_SUBMIT_CADENCE) {
                if let Some(stats) = recvr_stats.as_ref() {
                    stats.report();
                }
            }
            if let Err(send_err) = sendr.try_send(packet_batch) {
                match send_err {
                    crossbeam_channel::TrySendError::Full(v) => {
                        stats.overflow_shreds += v.len();
                    }
                    _ => unreachable!("EvictingSender holds on to both ends of the channel"),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn packet_modifier(
        receiver_thread_name: &'static str,
        modifier_thread_name: &'static str,
        sockets: Vec<Arc<UdpSocket>>,
        exit: Arc<AtomicBool>,
        sender: EvictingSender<PacketBatch>,
        recycler: PacketBatchRecycler,
        bank_forks: Arc<RwLock<BankForks>>,
        shred_version: u16,
        name: &'static str,
        receiver_name: &'static str,
        flags: PacketFlags,
        repair_context: Option<RepairContext>,
        turbine_disabled: Arc<AtomicBool>,
    ) -> (Vec<JoinHandle<()>>, JoinHandle<()>) {
        let (packet_sender, packet_receiver) =
            EvictingSender::new_bounded(SHRED_FETCH_CHANNEL_SIZE);
        let receiver_stats = Arc::new(StreamerReceiveStats::new(receiver_name));
        let streamers = sockets
            .into_iter()
            .enumerate()
            .map(|(i, socket)| {
                streamer::receiver(
                    format!("{receiver_thread_name}{i:02}"),
                    socket,
                    exit.clone(),
                    packet_sender.clone(),
                    recycler.clone(),
                    receiver_stats.clone(),
                    Some(Duration::from_millis(5)), // coalesce
                    true,                           // use_pinned_memory
                    None,                           // in_vote_only_mode
                    false,                          // is_staked_service
                )
            })
            .collect();
        let modifier_hdl = Builder::new()
            .name(modifier_thread_name.to_string())
            .spawn(move || {
                Self::modify_packets(
                    packet_receiver,
                    Some(receiver_stats),
                    sender,
                    &bank_forks,
                    shred_version,
                    name,
                    flags,
                    repair_context.as_ref(),
                    turbine_disabled,
                )
            })
            .unwrap();
        (streamers, modifier_hdl)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        sockets: Vec<Arc<UdpSocket>>,
        turbine_quic_endpoint_receiver: Receiver<(Pubkey, SocketAddr, Bytes)>,
        repair_response_quic_receiver: Receiver<(Pubkey, SocketAddr, Bytes)>,
        repair_socket: Arc<UdpSocket>,
        sender: EvictingSender<PacketBatch>,
        shred_version: u16,
        bank_forks: Arc<RwLock<BankForks>>,
        cluster_info: Arc<ClusterInfo>,
        outstanding_repair_requests: Arc<RwLock<OutstandingShredRepairs>>,
        turbine_disabled: Arc<AtomicBool>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let recycler = PacketBatchRecycler::warmed(100, 1024);
        let repair_context = RepairContext {
            repair_socket: repair_socket.clone(),
            cluster_info,
            outstanding_repair_requests,
        };

        let (mut tvu_threads, tvu_filter) = Self::packet_modifier(
            "solRcvrShred",
            "solTvuPktMod",
            sockets,
            exit.clone(),
            sender.clone(),
            recycler.clone(),
            bank_forks.clone(),
            shred_version,
            "shred_fetch",
            "shred_fetch_receiver",
            PacketFlags::empty(),
            None, // repair_context
            turbine_disabled.clone(),
        );

        let (repair_receiver, repair_handler) = Self::packet_modifier(
            "solRcvrShredRep",
            "solTvuRepPktMod",
            vec![repair_socket],
            exit.clone(),
            sender.clone(),
            recycler.clone(),
            bank_forks.clone(),
            shred_version,
            "shred_fetch_repair",
            "shred_fetch_repair_receiver",
            PacketFlags::REPAIR,
            Some(repair_context.clone()),
            turbine_disabled.clone(),
        );

        tvu_threads.extend(repair_receiver);
        tvu_threads.push(tvu_filter);
        tvu_threads.push(repair_handler);
        // Repair shreds fetched over QUIC protocol.
        {
            let (packet_sender, packet_receiver) = unbounded();
            let bank_forks = bank_forks.clone();
            let exit = exit.clone();
            let sender = sender.clone();
            let turbine_disabled = turbine_disabled.clone();
            tvu_threads.extend([
                Builder::new()
                    .name("solTvuRecvRpr".to_string())
                    .spawn(|| {
                        receive_quic_datagrams(
                            repair_response_quic_receiver,
                            PacketFlags::REPAIR,
                            packet_sender,
                            exit,
                        )
                    })
                    .unwrap(),
                Builder::new()
                    .name("solTvuFetchRpr".to_string())
                    .spawn(move || {
                        Self::modify_packets(
                            packet_receiver,
                            None,
                            sender,
                            &bank_forks,
                            shred_version,
                            "shred_fetch_repair_quic",
                            PacketFlags::REPAIR,
                            // No ping packets but need to verify repair nonce.
                            Some(&repair_context),
                            turbine_disabled,
                        )
                    })
                    .unwrap(),
            ]);
        }
        // Turbine shreds fetched over QUIC protocol.
        let (packet_sender, packet_receiver) = unbounded();
        tvu_threads.extend([
            Builder::new()
                .name("solTvuRecvQuic".to_string())
                .spawn(|| {
                    receive_quic_datagrams(
                        turbine_quic_endpoint_receiver,
                        PacketFlags::empty(),
                        packet_sender,
                        exit,
                    )
                })
                .unwrap(),
            Builder::new()
                .name("solTvuFetchQuic".to_string())
                .spawn(move || {
                    Self::modify_packets(
                        packet_receiver,
                        None,
                        sender,
                        &bank_forks,
                        shred_version,
                        "shred_fetch_quic",
                        PacketFlags::empty(),
                        None, // repair_context
                        turbine_disabled,
                    )
                })
                .unwrap(),
        ]);
        Self {
            thread_hdls: tvu_threads,
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }
}

impl RepairContext {
    fn keypair(&self) -> Arc<Keypair> {
        self.cluster_info.keypair()
    }
}

// Returns false if repair nonce is invalid and packet should be discarded.
#[must_use]
fn verify_repair_nonce(
    packet: PacketRef,
    now: u64, // solana_time_utils::timestamp()
    outstanding_repair_requests: &mut OutstandingShredRepairs,
) -> bool {
    debug_assert!(packet.meta().flags.contains(PacketFlags::REPAIR));
    let Some((shred, Some(nonce))) = shred::layout::get_shred_and_repair_nonce(packet) else {
        return false;
    };
    outstanding_repair_requests
        .register_response(nonce, shred, now, |_| ())
        .is_some()
}

pub(crate) fn receive_quic_datagrams(
    quic_datagrams_receiver: Receiver<(Pubkey, SocketAddr, Bytes)>,
    flags: PacketFlags,
    sender: Sender<PacketBatch>,
    exit: Arc<AtomicBool>,
) {
    const RECV_TIMEOUT: Duration = Duration::from_secs(1);
    const PACKET_COALESCE_DURATION: Duration = Duration::from_millis(1);
    while !exit.load(Ordering::Relaxed) {
        let entry = match quic_datagrams_receiver.recv_timeout(RECV_TIMEOUT) {
            Ok(entry) => entry,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let deadline = Instant::now() + PACKET_COALESCE_DURATION;
        let entries = std::iter::once(entry).chain(
            std::iter::repeat_with(|| quic_datagrams_receiver.recv_deadline(deadline).ok())
                .while_some(),
        );
        let packet_batch: BytesPacketBatch = entries
            .filter(|(_, _, bytes)| bytes.len() <= PACKET_DATA_SIZE)
            .map(|(_pubkey, addr, bytes)| {
                let meta = Meta {
                    size: bytes.len(),
                    addr: addr.ip(),
                    port: addr.port(),
                    flags,
                };
                BytesPacket::new(bytes, meta)
            })
            .collect();
        if !packet_batch.is_empty() && sender.send(packet_batch.into()).is_err() {
            return; // The receiver end of the channel is disconnected.
        }
    }
}

// Returns true if the feature is effective for the shred slot.
#[must_use]
#[allow(dead_code)]
fn check_feature_activation(
    feature: &Pubkey,
    shred_slot: Slot,
    feature_set: &FeatureSet,
    epoch_schedule: &EpochSchedule,
) -> bool {
    match feature_set.activated_slot(feature) {
        None => false,
        Some(feature_slot) => {
            let feature_epoch = epoch_schedule.get_epoch(feature_slot);
            let shred_epoch = epoch_schedule.get_epoch(shred_slot);
            feature_epoch < shred_epoch
        }
    }
}
