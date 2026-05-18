//! The `shred_fetch_stage` pulls shreds from UDP sockets and sends it to a channel.

use {
    crate::repair::{repair_service::OutstandingShredRepairs, serve_repair::ServeRepair},
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::shred::{
        self,
        filter::{ShredFilterContext, TurbineMode},
    },
    solana_perf::packet::{PacketBatch, PacketBatchRecycler, PacketFlags, PacketRef},
    solana_runtime::bank_forks::{BankForks, SharableBanks},
    solana_streamer::{
        evicting_sender::EvictingSender,
        streamer::{self, ChannelSend, PacketBatchReceiver, StreamerReceiveStats},
    },
    std::{
        net::UdpSocket,
        sync::{Arc, RwLock, atomic::AtomicBool},
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

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
        sharable_banks: &SharableBanks,
        shred_version: u16,
        name: &'static str,
        flags: PacketFlags,
        repair_context: Option<&RepairContext>,
        turbine_mode: TurbineMode,
    ) {
        // Only repair shreds need repair context.
        debug_assert_eq!(
            flags.contains(PacketFlags::REPAIR),
            repair_context.is_some()
        );
        const STATS_SUBMIT_CADENCE: Duration = Duration::from_secs(1);
        let mut shred_filter_ctx = ShredFilterContext::new_with_turbine_mode(
            sharable_banks.root(),
            shred_version,
            turbine_mode,
        );
        let repair_keypair = repair_context.map(RepairContext::keypair);

        for mut packet_batch in recvr {
            shred_filter_ctx.maybe_update(sharable_banks.root());
            shred_filter_ctx.stats.shred_count += packet_batch.len();

            if let Some(repair_context) = repair_context {
                debug_assert_eq!(flags, PacketFlags::REPAIR);
                debug_assert!(repair_keypair.is_some());
                if let Some(ref keypair) = repair_keypair {
                    ServeRepair::handle_repair_response_pings(
                        &repair_context.repair_socket,
                        keypair,
                        &mut packet_batch,
                        &mut shred_filter_ctx.stats,
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
            for mut packet in packet_batch.iter_mut().filter(|p| !p.meta().discard()) {
                if shred_filter_ctx.should_discard_packet(packet.as_ref()) {
                    packet.meta_mut().set_discard(true);
                } else {
                    packet.meta_mut().flags.insert(flags);
                }
            }
            if shred_filter_ctx.maybe_submit_stats(name, STATS_SUBMIT_CADENCE) {
                if let Some(stats) = recvr_stats.as_ref() {
                    stats.report();
                }
            }
            if let Err(send_err) = sendr.try_send(packet_batch) {
                match send_err {
                    crossbeam_channel::TrySendError::Full(v) => {
                        shred_filter_ctx.stats.overflow_shreds += v.len();
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
        turbine_mode: TurbineMode,
    ) -> (Vec<JoinHandle<()>>, JoinHandle<()>) {
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
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
                    &sharable_banks,
                    shred_version,
                    name,
                    flags,
                    repair_context.as_ref(),
                    turbine_mode,
                )
            })
            .unwrap();
        (streamers, modifier_hdl)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        sockets: Vec<Arc<UdpSocket>>,
        repair_socket: Arc<UdpSocket>,
        sender: EvictingSender<PacketBatch>,
        shred_version: u16,
        bank_forks: Arc<RwLock<BankForks>>,
        cluster_info: Arc<ClusterInfo>,
        outstanding_repair_requests: Arc<RwLock<OutstandingShredRepairs>>,
        turbine_mode: TurbineMode,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let recycler = PacketBatchRecycler::new();
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
            turbine_mode.clone(),
        );

        let (repair_receiver, repair_handler) = Self::packet_modifier(
            "solRcvrShredRep",
            "solTvuRepPktMod",
            vec![repair_socket],
            exit.clone(),
            sender.clone(),
            recycler,
            bank_forks.clone(),
            shred_version,
            "shred_fetch_repair",
            "shred_fetch_repair_receiver",
            PacketFlags::REPAIR,
            Some(repair_context.clone()),
            turbine_mode.clone(),
        );

        tvu_threads.extend(repair_receiver);
        tvu_threads.push(tvu_filter);
        tvu_threads.push(repair_handler);
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
