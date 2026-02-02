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
        sync::{atomic::AtomicBool, Arc},
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
    tokio::sync::mpsc::Sender as AsyncSender,
};

pub struct ServeRepairService {
    thread_hdls: Vec<JoinHandle<()>>,
}

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
        const REQUEST_CHANNEL_SIZE: usize = 4096;
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

// Adapts incoming UDP repair requests into RemoteRequest struct.
pub(crate) fn adapt_repair_requests_packets(
    packets_receiver: Receiver<PacketBatch>,
    remote_request_sender: Sender<RemoteRequest>,
) {
    for packets in packets_receiver {
        for packet in &packets {
            let Some(bytes) = packet.data(..).map(Vec::from) else {
                continue;
            };
            let request = RemoteRequest {
                remote_pubkey: None,
                remote_address: packet.meta().socket_addr(),
                bytes: Bytes::from(bytes),
            };
            if remote_request_sender.try_send(request).is_err() {
                // The receiver end of the channel is disconnected or full, discard this request.
                return;
            }
        }
    }
}
