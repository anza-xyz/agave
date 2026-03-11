use std::{
    net::{IpAddr, Ipv4Addr, UdpSocket},
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

pub enum CurrentSocket<'a> {
    Same(&'a UdpSocket),
    Changed(&'a UdpSocket),
}

pub trait SocketProvider {
    fn current_socket(&self) -> CurrentSocket<'_>;

    #[inline]
    fn current_socket_ref(&self) -> &UdpSocket {
        match self.current_socket() {
            CurrentSocket::Same(sock) | CurrentSocket::Changed(sock) => sock,
        }
    }
}

/// Fixed UDP Socket -> default
pub struct FixedSocketProvider {
    socket: Arc<UdpSocket>,
}
impl FixedSocketProvider {
    pub fn new(socket: Arc<UdpSocket>) -> Self {
        Self { socket }
    }
}
impl SocketProvider for FixedSocketProvider {
    #[inline]
    fn current_socket(&self) -> CurrentSocket<'_> {
        CurrentSocket::Same(self.socket.as_ref())
    }
}

pub struct MultihomedSocketProvider {
    sockets: Arc<[UdpSocket]>,
    bind_ip_addrs: Arc<BindIpAddrs>,
    last_index: AtomicUsize,
}

impl MultihomedSocketProvider {
    pub fn new(sockets: Arc<[UdpSocket]>, bind_ip_addrs: Arc<BindIpAddrs>) -> Self {
        Self {
            sockets,
            bind_ip_addrs,
            last_index: AtomicUsize::new(usize::MAX),
        }
    }
}

impl SocketProvider for MultihomedSocketProvider {
    #[inline]
    fn current_socket(&self) -> CurrentSocket<'_> {
        let idx = self.bind_ip_addrs.active_index();
        let last = self.last_index.swap(idx, Ordering::AcqRel);

        let sock = &self.sockets[idx];
        if last == idx {
            CurrentSocket::Same(sock)
        } else {
            CurrentSocket::Changed(sock)
        }
    }
}

#[derive(Debug, Clone)]
pub struct BindIpAddrs {
    /// The IP addresses this node may bind to
    /// Index 0 is the public internet address
    /// Index 1+ are secondary addresses (i.e. multihoming)
    addrs: Vec<IpAddr>,
    active_index: Arc<AtomicUsize>,
}

impl Default for BindIpAddrs {
    fn default() -> Self {
        Self::new(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]).unwrap()
    }
}

impl BindIpAddrs {
    pub fn new(addrs: Vec<IpAddr>) -> Result<Self, String> {
        if addrs.is_empty() {
            return Err(
                "BindIpAddrs requires at least one IP address (--bind-address)".to_string(),
            );
        }
        if addrs.len() > 1 {
            for ip in &addrs {
                if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
                    return Err(format!(
                        "Invalid configuration: {ip:?} is not allowed with multiple \
                         --bind-address values (loopback, unspecified, or multicast)"
                    ));
                }
            }
        }

        Ok(Self {
            addrs,
            active_index: Arc::new(AtomicUsize::new(0)),
        })
    }

    #[inline]
    pub fn active(&self) -> IpAddr {
        self.addrs[self.active_index.load(Ordering::Acquire)]
    }

    /// Change active to index (0 = public internet IP, 1+ = secondary IPs)
    pub fn set_active(&self, index: usize) -> Result<IpAddr, String> {
        if index >= self.addrs.len() {
            return Err(format!(
                "Index {index} out of range, only {} IPs available",
                self.addrs.len()
            ));
        }
        self.active_index.store(index, Ordering::Release);
        Ok(self.addrs[index])
    }

    #[inline]
    pub fn active_index(&self) -> usize {
        self.active_index.load(Ordering::Acquire)
    }

    #[inline]
    pub fn multihoming_enabled(&self) -> bool {
        self.addrs.len() > 1
    }
}

// Makes BindIpAddrs behave like &[IpAddr]
impl Deref for BindIpAddrs {
    type Target = [IpAddr];

    fn deref(&self) -> &Self::Target {
        &self.addrs
    }
}

// For generic APIs expecting something like AsRef<[IpAddr]>
impl AsRef<[IpAddr]> for BindIpAddrs {
    fn as_ref(&self) -> &[IpAddr] {
        &self.addrs
    }
}
