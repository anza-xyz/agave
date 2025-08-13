use std::{
    net::{IpAddr, Ipv4Addr},
    ops::Deref,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[derive(Debug, Clone)]
pub struct BindIpAddrs {
    /// The IP addresses this node may bind to
    /// Index 0 is the primary address
    /// Index 1+ are secondary addresses
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
                        "Invalid configuration: {:?} is not allowed with multiple --bind-address values (loopback, unspecified, or multicast)",
                        ip
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

    /// Change active to index (0 = primary)
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
