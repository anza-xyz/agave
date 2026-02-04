use {
    crate::repair::{quic_endpoint::RemoteRequest, serve_repair::ServeRepair},
    bytes::Bytes,
    crossbeam_channel::{bounded, Receiver, Sender},
    solana_net_utils::SocketAddrSpace,
    solana_perf::{packet::PacketBatch, recycler::Recycler},
    solana_streamer::{
        evicting_sender::EvictingSender,
        streamer::{self, StreamerReceiveStats},
    },
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
    tokio::sync::mpsc::Sender as AsyncSender,
};

pub struct ServeRepairService {
    thread_hdls: Vec<JoinHandle<()>>,
}

pub(crate) const REQUEST_CHANNEL_SIZE: usize = 4096;

impl ServeRepairService {
    pub(crate) fn new(
        serve_repair: ServeRepair,
        remote_request_sender: Sender<RemoteRequest>,
        remote_request_receiver: Receiver<RemoteRequest>,
        repair_response_quic_sender: AsyncSender<(SocketAddr, Bytes)>,
        serve_repair_socket: UdpSocket,
        socket_addr_space: SocketAddrSpace,
        stats_reporter_sender: Sender<Box<dyn FnOnce() + Send>>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let (request_sender, request_receiver) = EvictingSender::new_bounded(REQUEST_CHANNEL_SIZE);
        let serve_repair_socket = Arc::new(serve_repair_socket);
        let t_receiver = streamer::receiver(
            "solRcvrServeRep".to_string(),
            serve_repair_socket.clone(),
            exit.clone(),
            request_sender,
            Recycler::default(),
            Arc::new(StreamerReceiveStats::new("serve_repair_receiver")),
            Some(Duration::from_millis(1)), // coalesce
            false,                          // use_pinned_memory
            false,                          // is_staked_service
        );
        let t_packet_adapter = Builder::new()
            .name(String::from("solServRAdapt"))
            .spawn(|| adapt_repair_requests_packets(request_receiver, remote_request_sender))
            .unwrap();
        // NOTE: we use a larger sending channel here compared to the receiving one.
        //
        // That's because by the time we're done with the work to compute the repair packets,
        // discarding the packet because of a full channel seems like a waste. For that reason the
        // push to this channel is blocking and having more space here gives the sending thread an
        // much greater chance to get to pulling from this channel before the channel fills up.
        let (response_sender, response_receiver) = bounded(3 * REQUEST_CHANNEL_SIZE);
        let t_responder = streamer::responder(
            "Repair",
            serve_repair_socket,
            response_receiver,
            socket_addr_space,
            Some(stats_reporter_sender),
        );
        let t_listen = serve_repair.listen(
            remote_request_receiver,
            response_sender,
            repair_response_quic_sender,
            exit,
        );

        let thread_hdls = vec![t_receiver, t_packet_adapter, t_responder, t_listen];
        Self { thread_hdls }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.thread_hdls.into_iter().try_for_each(JoinHandle::join)
    }
}

static DROPPED_PACKETS: AtomicUsize = AtomicUsize::new(0);
static DROPPED_BATCHES: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn report_stats() {
    let dropped_batches = DROPPED_BATCHES.load(Ordering::Relaxed);
    let dropped_packets = DROPPED_PACKETS.load(Ordering::Relaxed);
    datapoint_info!(
        "adapt-repair-request-packets",
        ("dropped_packets", dropped_packets as i64, i64),
        ("dropped_batches", dropped_batches as i64, i64),
    );
}

// Adapts incoming UDP repair requests into RemoteRequest struct.
pub(crate) fn adapt_repair_requests_packets(
    packets_receiver: Receiver<PacketBatch>,
    remote_request_sender: Sender<RemoteRequest>,
) {
    'recv_batch: for packets in packets_receiver {
        let total_packets = packets.len();
        for (i, packet) in packets.iter().enumerate() {
            let Some(bytes) = packet.data(..).map(Vec::from) else {
                continue;
            };
            let request = RemoteRequest {
                remote_pubkey: None,
                remote_address: packet.meta().socket_addr(),
                bytes: Bytes::from(bytes),
            };
            match remote_request_sender.try_send(request) {
                Ok(_) => {}
                Err(crossbeam_channel::TrySendError::Full(_)) => {
                    DROPPED_BATCHES.fetch_add(1, Ordering::Relaxed);
                    DROPPED_PACKETS.fetch_add(total_packets - i, Ordering::Relaxed);
                    continue 'recv_batch;
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
            }
        }
    }
}
