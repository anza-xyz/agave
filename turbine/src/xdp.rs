// re-export since this is needed at validator startup
pub use agave_xdp::set_cpu_affinity;
#[cfg(target_os = "linux")]
use {
    agave_xdp::{
        device::{NetworkDevice, QueueId},
        load_xdp_program,
        tx_loop::tx_loop,
    },
    crossbeam_channel::TryRecvError,
    std::{thread::Builder, time::Duration},
};
use {
    crossbeam_channel::{Sender, TrySendError},
    solana_ledger::shred,
    std::{error::Error, net::SocketAddr, sync::Arc, thread},
};

#[derive(Clone, Debug)]
pub struct XdpConfig {
    pub interface: Option<String>,
    pub cpus: Vec<usize>,
    pub zero_copy: bool,
    // The capacity of the channel that sits between retransmit stage and each XDP thread that
    // enqueues packets to the NIC.
    pub rtx_channel_cap: usize,
}

impl XdpConfig {
    // A nice round number
    const DEFAULT_RTX_CHANNEL_CAP: usize = 1_000_000;
}

impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            interface: None,
            cpus: vec![],
            zero_copy: false,
            rtx_channel_cap: Self::DEFAULT_RTX_CHANNEL_CAP,
        }
    }
}

impl XdpConfig {
    pub fn new(interface: Option<impl Into<String>>, cpus: Vec<usize>, zero_copy: bool) -> Self {
        Self {
            interface: interface.map(|s| s.into()),
            cpus,
            zero_copy,
            rtx_channel_cap: XdpConfig::DEFAULT_RTX_CHANNEL_CAP,
        }
    }
}

/// The shred payload variants of the Xdp channel.
///
/// This is currently meant to capture the constraints of both the retransmit
/// and broadcast stages.
pub(crate) enum XdpShredPayload {
    /// The shreds, and thus their payloads, are owned by the caller.
    ///
    /// For example, retransmit has its own [`Vec`] of [`shred::Shred`], and can simply
    /// pass along the payloads to the XDP thread(s) via the Xdp channel.
    Owned(shred::Payload),
    /// The shreds, and thus their payloads, are shared between disparate components in the validator.
    ///
    /// For example, broadcast deals with an `Arc<Vec<shred::Shred>>` due to those shreds being
    /// shared with the blockstore (see [`StandardBroadcastRun::process_receive_results`](crate::broadcast_stage::standard_broadcast_run::StandardBroadcastRun::process_receive_results)).
    /// To avoid cloning the payloads, we pass along the `Arc` reference and the index of the shred
    /// in the `Vec` to the XDP thread(s).
    Shared {
        ptr: Arc<Vec<shred::Shred>>,
        index: usize,
    },
}

impl AsRef<[u8]> for XdpShredPayload {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self {
            XdpShredPayload::Owned(payload) => payload.as_ref(),
            XdpShredPayload::Shared { ptr, index } => ptr[*index].payload().as_ref(),
        }
    }
}

#[derive(Clone)]
pub struct XdpSender {
    senders: Vec<Sender<(Vec<SocketAddr>, XdpShredPayload)>>,
}

impl XdpSender {
    #[inline]
    pub(crate) fn try_send(
        &self,
        sender_index: usize,
        addr: Vec<SocketAddr>,
        payload: XdpShredPayload,
    ) -> Result<(), TrySendError<(Vec<SocketAddr>, XdpShredPayload)>> {
        self.senders[sender_index % self.senders.len()].try_send((addr, payload))
    }
}

pub struct XdpRetransmitter {
    threads: Vec<thread::JoinHandle<()>>,
}

impl XdpRetransmitter {
    #[cfg(not(target_os = "linux"))]
    pub fn new(_config: XdpConfig, _src_port: u16) -> Result<(Self, XdpSender), Box<dyn Error>> {
        Err("XDP is only supported on Linux".into())
    }

    #[cfg(target_os = "linux")]
    pub fn new(config: XdpConfig, src_port: u16) -> Result<(Self, XdpSender), Box<dyn Error>> {
        use caps::{
            CapSet,
            Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW},
        };

        // switch to higher caps while we setup XDP. We assume that an error in
        // this function is irrecoverable so we don't try to drop on errors.
        for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF] {
            caps::raise(None, CapSet::Effective, cap)
                .map_err(|e| format!("failed to raise {cap:?} capability: {e}"))?;
        }

        let dev = Arc::new(if let Some(interface) = config.interface {
            NetworkDevice::new(interface).unwrap()
        } else {
            NetworkDevice::new_from_default_route().unwrap()
        });

        let ebpf = if config.zero_copy {
            Some(
                load_xdp_program(dev.if_index())
                    .map_err(|e| format!("failed to attach xdp program: {e}"))?,
            )
        } else {
            None
        };

        for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF] {
            caps::drop(None, CapSet::Effective, cap).unwrap();
        }

        let (senders, receivers) = (0..config.cpus.len())
            .map(|_| crossbeam_channel::bounded(config.rtx_channel_cap))
            .unzip::<_, _, Vec<_>, Vec<_>>();

        let mut threads = vec![];

        let (drop_sender, drop_receiver) = crossbeam_channel::bounded(1_000_000);
        threads.push(
            Builder::new()
                .name("solRetransmDrop".to_owned())
                .spawn(move || {
                    loop {
                        // drop shreds in a dedicated thread so that we never lock/madvise() from
                        // the xdp thread
                        match drop_receiver.try_recv() {
                            Ok(i) => {
                                drop(i);
                            }
                            Err(TryRecvError::Empty) => {
                                thread::sleep(Duration::from_millis(1));
                            }
                            Err(TryRecvError::Disconnected) => break,
                        }
                    }
                    // move the ebpf program here so it stays attached until we exit
                    drop(ebpf);
                })
                .unwrap(),
        );

        for (i, (receiver, cpu_id)) in receivers
            .into_iter()
            .zip(config.cpus.into_iter())
            .enumerate()
        {
            let dev = Arc::clone(&dev);
            let drop_sender = drop_sender.clone();
            threads.push(
                Builder::new()
                    .name(format!("solRetransmIO{i:02}"))
                    .spawn(move || {
                        tx_loop(
                            cpu_id,
                            &dev,
                            QueueId(i as u64),
                            config.zero_copy,
                            None,
                            None,
                            src_port,
                            None,
                            receiver,
                            drop_sender,
                        )
                    })
                    .unwrap(),
            );
        }

        Ok((Self { threads }, XdpSender { senders }))
    }

    pub fn join(self) -> thread::Result<()> {
        for handle in self.threads {
            handle.join()?;
        }
        Ok(())
    }
}
