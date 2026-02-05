use {
    crate::repair::{quic_endpoint::RemoteRequest, serve_repair::ServeRepair},
    bytes::Bytes,
    crossbeam_channel::{unbounded, Receiver, Sender},
    solana_net_utils::SocketAddrSpace,
    solana_perf::{packet::PacketBatch, recycler::Recycler},
    solana_streamer::streamer::{self, StreamerReceiveStats},
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
        serve_repair_socket: UdpSocket,
        socket_addr_space: SocketAddrSpace,
        stats_reporter_sender: Sender<Box<dyn FnOnce() + Send>>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let (request_sender, request_receiver) = unbounded();
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
        let (response_sender, response_receiver) = unbounded();
        let t_responder = streamer::responder(
            "Repair",
            serve_repair_socket,
            response_receiver,
            socket_addr_space,
            Some(stats_reporter_sender),
        );
        let t_listen = serve_repair.listen(request_receiver, response_sender, exit);

        let thread_hdls = vec![t_receiver, t_packet_adapter, t_responder, t_listen];
        Self { thread_hdls }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.thread_hdls.into_iter().try_for_each(JoinHandle::join)
    }
}
