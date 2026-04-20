use {
    crate::repair::serve_repair::ServeRepair,
    crossbeam_channel::{Sender, bounded},
    solana_net_utils::SocketAddrSpace,
    solana_perf::recycler::Recycler,
    solana_streamer::{
        evicting_sender::EvictingSender,
        streamer::{self, StreamerReceiveStats},
    },
    std::{
        net::UdpSocket,
        sync::{Arc, atomic::AtomicBool},
        thread::{self, JoinHandle},
        time::Duration,
    },
};

pub struct ServeRepairService {
    thread_hdls: Vec<JoinHandle<()>>,
}

/// Repair request channel size. Grossly overprovisioned compared to actual needs (~1024 would be sufficient).
pub(crate) const REQUEST_CHANNEL_SIZE: usize = 4096;

/// Repair response channel size. Grossly overprovisioned compared to actual needs (~256 would be sufficient).
pub(crate) const RESPONSE_CHANNEL_SIZE: usize = REQUEST_CHANNEL_SIZE;

impl ServeRepairService {
    pub(crate) fn new(
        serve_repair: ServeRepair,
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
        let (response_sender, response_receiver) = bounded(RESPONSE_CHANNEL_SIZE);
        let t_responder = streamer::responder(
            "Repair",
            serve_repair_socket,
            response_receiver,
            socket_addr_space,
            Some(stats_reporter_sender),
        );
        let t_listen = serve_repair.listen(request_receiver, response_sender, exit);

        let thread_hdls = vec![t_receiver, t_responder, t_listen];
        Self { thread_hdls }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.thread_hdls.into_iter().try_for_each(JoinHandle::join)
    }
}
