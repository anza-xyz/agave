pub use solana_client::connection_cache::Protocol;
use {
    crate::{
        crds_data::MAX_WALLCLOCK,
        define_tlv_enum,
        tlv::{self, TlvDecodeError, TlvRecord},
    },
    assert_matches::{assert_matches, debug_assert_matches},
    serde::{Deserialize, Deserializer, Serialize},
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    solana_serde_varint as serde_varint, solana_short_vec as short_vec,
    static_assertions::const_assert_eq,
    std::{
        cmp::Ordering,
        collections::HashSet,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        time::{SystemTime, UNIX_EPOCH},
    },
    thiserror::Error,
};

const DEFAULT_RPC_PORT: u16 = 8899;
const DEFAULT_RPC_PUBSUB_PORT: u16 = 8900;

pub const SOCKET_ADDR_UNSPECIFIED: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), /*port:*/ 0u16);
const EMPTY_SOCKET_ADDR_CACHE: [SocketAddr; SOCKET_CACHE_SIZE] =
    [SOCKET_ADDR_UNSPECIFIED; SOCKET_CACHE_SIZE];

const SOCKET_TAG_GOSSIP: u8 = 0;
const SOCKET_TAG_RPC: u8 = 2;
const SOCKET_TAG_RPC_PUBSUB: u8 = 3;
const SOCKET_TAG_SERVE_REPAIR: u8 = 4;
const SOCKET_TAG_SERVE_REPAIR_QUIC: u8 = 1;
const SOCKET_TAG_TPU: u8 = 5;
const SOCKET_TAG_TPU_FORWARDS: u8 = 6;
const SOCKET_TAG_TPU_FORWARDS_QUIC: u8 = 7;
const SOCKET_TAG_TPU_QUIC: u8 = 8;
const SOCKET_TAG_TPU_VOTE: u8 = 9;
const SOCKET_TAG_TPU_VOTE_QUIC: u8 = 12;
const SOCKET_TAG_TVU: u8 = 10;
const SOCKET_TAG_TVU_QUIC: u8 = 11;
const SOCKET_TAG_ALPENGLOW: u8 = 13;
const_assert_eq!(SOCKET_CACHE_SIZE, 14);
const SOCKET_CACHE_SIZE: usize = SOCKET_TAG_ALPENGLOW as usize + 1usize;

// An alias for a function that reads data from a ContactInfo entry stored in
// the gossip CRDS table.
pub trait ContactInfoQuery<R>: Fn(&ContactInfo) -> R {}
impl<R, F: Fn(&ContactInfo) -> R> ContactInfoQuery<R> for F {}

#[derive(Copy, Clone, Debug, Eq, Error, PartialEq)]
pub enum Error {
    #[error("Duplicate IP address: {0}")]
    DuplicateIpAddr(IpAddr),
    #[error("Duplicate socket: {0}")]
    DuplicateSocket(/*key:*/ u8),
    #[error("Invalid IP address index: {index}, num addrs: {num_addrs}")]
    InvalidIpAddrIndex { index: u8, num_addrs: usize },
    #[error("Invalid port: {0}")]
    InvalidPort(/*port:*/ u16),
    #[error("Invalid {0:?} (udp) and {1:?} (quic) sockets")]
    InvalidQuicSocket(Option<SocketAddr>, Option<SocketAddr>),
    #[error("IP addresses saturated")]
    IpAddrsSaturated,
    #[error("Multicast IP address: {0}")]
    MulticastIpAddr(IpAddr),
    #[error("Port offsets overflow")]
    PortOffsetsOverflow,
    #[error("Socket not found: {0}")]
    SocketNotFound(/*key:*/ u8),
    #[error("Unspecified IP address: {0}")]
    UnspecifiedIpAddr(IpAddr),
    #[error("Unused IP address: {0}")]
    UnusedIpAddr(IpAddr),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContactInfo {
    pubkey: Pubkey,
    #[serde(with = "serde_varint")]
    wallclock: u64,
    // When the node instance was first created.
    // Identifies duplicate running instances.
    outset: u64,
    shred_version: u16,
    version: solana_version::Version,
    // All IP addresses are unique and referenced at least once in sockets.
    #[serde(with = "short_vec")]
    addrs: Vec<IpAddr>,
    // All sockets have a unique key and a valid IP address index.
    #[serde(with = "short_vec")]
    sockets: Vec<SocketEntry>,
    #[serde(with = "short_vec")]
    extensions: Vec<Extension>,
    // Only sanitized socket-addrs can be cached!
    #[serde(skip_serializing)]
    cache: [SocketAddr; SOCKET_CACHE_SIZE],
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
struct SocketEntry {
    key: u8,   // Protocol identifier, e.g. tvu, tpu, etc
    index: u8, // IpAddr index in the accompanying addrs vector.
    #[serde(with = "serde_varint")]
    offset: u16, // Port offset with respect to the previous entry.
}

define_tlv_enum!(
    /// TLV encoded Extensions in ContactInfo messages
    ///
    /// On the wire each record is: [type: u8][len: varint][bytes]
    /// Extensions with unknown types are skipped by tlv::parse,
    /// so new types can be added without breaking legacy code,
    /// and support by all clients is not required.
    ///
    /// Always add new TLV records to the end of this enum.
    /// Never reorder or reuse a type.
    /// Ensure new type collisions do not happen.
    pub(crate) enum Extension {}
);

// As part of deserialization, self.addrs and self.sockets should be cross
// verified and self.cache needs to be populated. This type serves as a
// workaround since serde does not have an initializer.
// https://github.com/serde-rs/serde/issues/642
#[derive(Deserialize)]
struct ContactInfoLite {
    pubkey: Pubkey,
    #[serde(with = "serde_varint")]
    wallclock: u64,
    outset: u64,
    shred_version: u16,
    version: solana_version::Version,
    #[serde(with = "short_vec")]
    addrs: Vec<IpAddr>,
    #[serde(with = "short_vec")]
    sockets: Vec<SocketEntry>,
    #[allow(dead_code)]
    #[serde(with = "short_vec")]
    extensions: Vec<TlvRecord>,
}

macro_rules! get_socket {
    ($name:ident, $key:ident) => {
        #[inline]
        pub fn $name(&self) -> Option<SocketAddr> {
            let socket = &self.cache[usize::from($key)];
            (socket != &SOCKET_ADDR_UNSPECIFIED)
                .then_some(socket)
                .copied()
        }
    };
    ($name:ident, $udp:ident, $quic:ident) => {
        #[inline]
        pub fn $name(&self, protocol: Protocol) -> Option<SocketAddr> {
            let key = match protocol {
                Protocol::QUIC => $quic,
                Protocol::UDP => $udp,
            };
            let socket = &self.cache[usize::from(key)];
            (socket != &SOCKET_ADDR_UNSPECIFIED)
                .then_some(socket)
                .copied()
        }
    };
}

macro_rules! set_socket {
    ($name:ident, $key:ident) => {
        pub fn $name<T>(&mut self, socket: T) -> Result<(), Error>
        where
            SocketAddr: From<T>,
        {
            let socket = SocketAddr::from(socket);
            self.set_socket($key, socket)
        }
    };
    ($name:ident, $udp:ident, $quic:ident) => {
        pub fn $name<T>(&mut self, protocol: Protocol, socket: T) -> Result<(), Error>
        where
            SocketAddr: From<T>,
        {
            let socket = SocketAddr::from(socket);
            let key = match protocol {
                Protocol::UDP => $udp,
                Protocol::QUIC => $quic,
            };
            self.set_socket(key, socket)
        }
    };
}

macro_rules! remove_socket {
    ($name:ident, $key:ident) => {
        pub fn $name(&mut self) {
            self.remove_socket($key);
        }
    };
    ($name:ident, $key:ident, $quic:ident) => {
        pub fn $name(&mut self) {
            self.remove_socket($key);
            self.remove_socket($quic);
        }
    };
}

impl ContactInfo {
    pub fn new(pubkey: Pubkey, wallclock: u64, shred_version: u16) -> Self {
        Self {
            pubkey,
            wallclock,
            outset: get_node_outset(),
            shred_version,
            version: solana_version::Version::default(),
            addrs: Vec::<IpAddr>::default(),
            sockets: Vec::<SocketEntry>::default(),
            extensions: Vec::default(),
            cache: EMPTY_SOCKET_ADDR_CACHE,
        }
    }

    #[inline]
    pub fn pubkey(&self) -> &Pubkey {
        &self.pubkey
    }

    #[inline]
    pub fn wallclock(&self) -> u64 {
        self.wallclock
    }

    #[inline]
    pub fn shred_version(&self) -> u16 {
        self.shred_version
    }

    #[inline]
    pub(crate) fn version(&self) -> &solana_version::Version {
        &self.version
    }

    pub fn hot_swap_pubkey(&mut self, pubkey: Pubkey) {
        self.pubkey = pubkey;
        // Need to update ContactInfo.outset so that this node's contact-info
        // will override older node with the same pubkey.
        self.outset = get_node_outset();
    }

    pub fn set_wallclock(&mut self, wallclock: u64) {
        self.wallclock = wallclock;
    }

    pub fn set_shred_version(&mut self, shred_version: u16) {
        self.shred_version = shred_version
    }

    get_socket!(gossip, SOCKET_TAG_GOSSIP);
    get_socket!(rpc, SOCKET_TAG_RPC);
    get_socket!(rpc_pubsub, SOCKET_TAG_RPC_PUBSUB);
    get_socket!(
        serve_repair,
        SOCKET_TAG_SERVE_REPAIR,
        SOCKET_TAG_SERVE_REPAIR_QUIC
    );
    get_socket!(tpu, SOCKET_TAG_TPU, SOCKET_TAG_TPU_QUIC);
    get_socket!(
        tpu_forwards,
        SOCKET_TAG_TPU_FORWARDS,
        SOCKET_TAG_TPU_FORWARDS_QUIC
    );
    get_socket!(tpu_vote, SOCKET_TAG_TPU_VOTE, SOCKET_TAG_TPU_VOTE_QUIC);
    get_socket!(tvu, SOCKET_TAG_TVU, SOCKET_TAG_TVU_QUIC);
    get_socket!(alpenglow, SOCKET_TAG_ALPENGLOW);

    set_socket!(set_gossip, SOCKET_TAG_GOSSIP);
    set_socket!(set_rpc, SOCKET_TAG_RPC);
    set_socket!(set_rpc_pubsub, SOCKET_TAG_RPC_PUBSUB);
    set_socket!(set_tpu, SOCKET_TAG_TPU, SOCKET_TAG_TPU_QUIC);
    set_socket!(
        set_tpu_forwards,
        SOCKET_TAG_TPU_FORWARDS,
        SOCKET_TAG_TPU_FORWARDS_QUIC
    );
    set_socket!(
        set_serve_repair,
        SOCKET_TAG_SERVE_REPAIR,
        SOCKET_TAG_SERVE_REPAIR_QUIC
    );
    set_socket!(set_tpu_vote, SOCKET_TAG_TPU_VOTE, SOCKET_TAG_TPU_VOTE_QUIC);
    set_socket!(set_tvu, SOCKET_TAG_TVU, SOCKET_TAG_TVU_QUIC);
    set_socket!(set_alpenglow, SOCKET_TAG_ALPENGLOW);

    remove_socket!(
        remove_serve_repair,
        SOCKET_TAG_SERVE_REPAIR,
        SOCKET_TAG_SERVE_REPAIR_QUIC
    );
    remove_socket!(remove_tpu, SOCKET_TAG_TPU, SOCKET_TAG_TPU_QUIC);
    remove_socket!(
        remove_tpu_forwards,
        SOCKET_TAG_TPU_FORWARDS,
        SOCKET_TAG_TPU_FORWARDS_QUIC
    );
    remove_socket!(remove_tvu, SOCKET_TAG_TVU, SOCKET_TAG_TVU_QUIC);
    remove_socket!(remove_alpenglow, SOCKET_TAG_ALPENGLOW);

    #[cfg(test)]
    fn get_socket(&self, key: u8) -> Result<SocketAddr, Error> {
        let mut port = 0u16;
        for entry in &self.sockets {
            port += entry.offset;
            if entry.key == key {
                let addr =
                    self.addrs
                        .get(usize::from(entry.index))
                        .ok_or(Error::InvalidIpAddrIndex {
                            index: entry.index,
                            num_addrs: self.addrs.len(),
                        })?;
                let socket = SocketAddr::new(*addr, port);
                sanitize_socket(&socket)?;
                return Ok(socket);
            }
        }
        Err(Error::SocketNotFound(key))
    }

    // Adds given IP address to self.addrs returning respective index.
    fn push_addr(&mut self, addr: IpAddr) -> Result<u8, Error> {
        match self.addrs.iter().position(|k| k == &addr) {
            Some(index) => u8::try_from(index).map_err(|_| Error::IpAddrsSaturated),
            None => {
                let index = u8::try_from(self.addrs.len()).map_err(|_| Error::IpAddrsSaturated)?;
                self.addrs.push(addr);
                Ok(index)
            }
        }
    }

    pub fn set_socket(&mut self, key: u8, socket: SocketAddr) -> Result<(), Error> {
        sanitize_socket(&socket)?;
        // Remove the old entry associated with this key (if any).
        self.remove_socket(key);
        // Find the index at which the new socket entry would be inserted into
        // self.sockets, and the respective port offset.
        let mut offset = socket.port();
        let index = self.sockets.iter().position(|entry| {
            offset = match offset.checked_sub(entry.offset) {
                None => return true,
                Some(offset) => offset,
            };
            false
        });
        let entry = SocketEntry {
            key,
            index: self.push_addr(socket.ip())?,
            offset,
        };
        // Insert the new entry into self.sockets.
        // Adjust the port offset of the next entry (if any).
        match index {
            None => self.sockets.push(entry),
            Some(index) => {
                self.sockets[index].offset -= entry.offset;
                self.sockets.insert(index, entry);
            }
        }
        if let Some(entry) = self.cache.get_mut(usize::from(key)) {
            *entry = socket; // socket is already sanitized above.
        }
        debug_assert_matches!(sanitize_entries(&self.addrs, &self.sockets), Ok(()));
        Ok(())
    }

    // Removes the socket associated with the specified key.
    fn remove_socket(&mut self, key: u8) {
        if let Some(index) = self.sockets.iter().position(|entry| entry.key == key) {
            let entry = self.sockets.remove(index);
            if let Some(next_entry) = self.sockets.get_mut(index) {
                next_entry.offset += entry.offset;
            }
            self.maybe_remove_addr(entry.index);
            if let Some(entry) = self.cache.get_mut(usize::from(key)) {
                *entry = SOCKET_ADDR_UNSPECIFIED;
            }
        }
    }

    // Removes the IP address at the given index if
    // no socket entry references that index.
    fn maybe_remove_addr(&mut self, index: u8) {
        if !self.sockets.iter().any(|entry| entry.index == index) {
            self.addrs.remove(usize::from(index));
            for entry in &mut self.sockets {
                if entry.index > index {
                    entry.index -= 1;
                }
            }
        }
    }

    /// port must not be 0
    /// ip must be specified and not multicast
    pub fn is_valid_address(addr: &SocketAddr, socket_addr_space: &SocketAddrSpace) -> bool {
        addr.port() != 0u16 && Self::is_valid_ip(addr.ip()) && socket_addr_space.check(addr)
    }

    fn is_valid_ip(addr: IpAddr) -> bool {
        !(addr.is_unspecified() || addr.is_multicast())
    }

    /// New random ContactInfo for tests and simulations.
    pub fn new_rand<R: rand::Rng>(rng: &mut R, pubkey: Option<Pubkey>) -> Self {
        let delay = 10 * 60 * 1000; // 10 minutes
        let now = solana_time_utils::timestamp() - delay + rng.gen_range(0..2 * delay);
        let pubkey = pubkey.unwrap_or_else(solana_pubkey::new_rand);
        let mut node = ContactInfo::new_localhost(&pubkey, now);
        let _ = node.set_gossip((Ipv4Addr::LOCALHOST, rng.gen_range(1024..u16::MAX)));
        node
    }

    /// Construct a ContactInfo that's only usable for gossip
    pub fn new_gossip_entry_point(gossip_addr: &SocketAddr) -> Self {
        let mut node = Self::new(
            Pubkey::default(),
            solana_time_utils::timestamp(), // wallclock
            0,                              // shred_version
        );
        if let Err(err) = node.set_gossip(*gossip_addr) {
            error!("Invalid entrypoint: {gossip_addr}, {err:?}");
        }
        node
    }

    // Only for tests and simulations.
    pub fn new_localhost(pubkey: &Pubkey, wallclock: u64) -> Self {
        use Protocol::{QUIC, UDP};
        let mut node = Self::new(*pubkey, wallclock, /*shred_version:*/ 0u16);
        node.set_gossip((Ipv4Addr::LOCALHOST, 8000)).unwrap();
        node.set_tvu(UDP, (Ipv4Addr::LOCALHOST, 8001)).unwrap();
        node.set_tvu(QUIC, (Ipv4Addr::LOCALHOST, 8002)).unwrap();
        node.set_tpu(UDP, (Ipv4Addr::LOCALHOST, 8003)).unwrap();
        node.set_tpu(QUIC, (Ipv4Addr::LOCALHOST, 8009)).unwrap();
        node.set_tpu_forwards(UDP, (Ipv4Addr::LOCALHOST, 8004))
            .unwrap();
        node.set_tpu_forwards(QUIC, (Ipv4Addr::LOCALHOST, 8010))
            .unwrap();
        node.set_tpu_vote(UDP, (Ipv4Addr::LOCALHOST, 8005)).unwrap();
        node.set_tpu_vote(QUIC, (Ipv4Addr::LOCALHOST, 8007))
            .unwrap();
        node.set_rpc((Ipv4Addr::LOCALHOST, DEFAULT_RPC_PORT))
            .unwrap();
        node.set_rpc_pubsub((Ipv4Addr::LOCALHOST, DEFAULT_RPC_PUBSUB_PORT))
            .unwrap();
        node.set_serve_repair(UDP, (Ipv4Addr::LOCALHOST, 8008))
            .unwrap();
        node.set_serve_repair(QUIC, (Ipv4Addr::LOCALHOST, 8006))
            .unwrap();
        node
    }

    // Only for tests and simulations.
    pub fn new_with_socketaddr(pubkey: &Pubkey, socket: &SocketAddr) -> Self {
        use Protocol::{QUIC, UDP};
        assert_matches!(sanitize_socket(socket), Ok(()));
        let mut node = Self::new(
            *pubkey,
            solana_time_utils::timestamp(), // wallclock,
            0u16,                           // shred_version
        );
        let (addr, port) = (socket.ip(), socket.port());
        node.set_gossip((addr, port + 1)).unwrap();
        node.set_tvu(UDP, (addr, port + 2)).unwrap();
        node.set_tvu(QUIC, (addr, port + 3)).unwrap();
        node.set_tpu(UDP, (addr, port)).unwrap();
        node.set_tpu(QUIC, (addr, port + 6)).unwrap();
        node.set_tpu_forwards(UDP, (addr, port + 5)).unwrap();
        node.set_tpu_forwards(QUIC, (addr, port + 11)).unwrap();
        node.set_tpu_vote(UDP, (addr, port + 7)).unwrap();
        node.set_tpu_vote(QUIC, (addr, port + 9)).unwrap();
        node.set_rpc((addr, DEFAULT_RPC_PORT)).unwrap();
        node.set_rpc_pubsub((addr, DEFAULT_RPC_PUBSUB_PORT))
            .unwrap();
        node.set_serve_repair(UDP, (addr, port + 8)).unwrap();
        node.set_serve_repair(QUIC, (addr, port + 4)).unwrap();
        node
    }

    // Returns true if the other contact-info is a duplicate instance of this
    // node, with a more recent `outset` timestamp.
    #[inline]
    #[must_use]
    pub(crate) fn check_duplicate(&self, other: &ContactInfo) -> bool {
        self.pubkey == other.pubkey && self.outset < other.outset
    }

    // Returns None if the contact-infos have different pubkey.
    // Otherwise returns true if (self.outset, self.wallclock) tuple is larger
    // than (other.outset, other.wallclock).
    // If the tuples are equal it returns None.
    #[inline]
    #[must_use]
    pub(crate) fn overrides(&self, other: &ContactInfo) -> Option<bool> {
        if self.pubkey != other.pubkey {
            return None;
        }
        let other = (other.outset, other.wallclock);
        match (self.outset, self.wallclock).cmp(&other) {
            Ordering::Less => Some(false),
            Ordering::Greater => Some(true),
            Ordering::Equal => None,
        }
    }
}

fn get_node_outset() -> u64 {
    let now = SystemTime::now();
    let elapsed = now.duration_since(UNIX_EPOCH).unwrap();
    u64::try_from(elapsed.as_micros()).unwrap()
}

impl Default for ContactInfo {
    fn default() -> Self {
        Self::new(
            Pubkey::default(),
            0, // wallclock
            0, // shred_version
        )
    }
}

impl<'de> Deserialize<'de> for ContactInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let node = ContactInfoLite::deserialize(deserializer)?;
        ContactInfo::try_from(node).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<ContactInfoLite> for ContactInfo {
    type Error = Error;

    fn try_from(node: ContactInfoLite) -> Result<Self, Self::Error> {
        let ContactInfoLite {
            pubkey,
            wallclock,
            outset,
            shred_version,
            version,
            addrs,
            sockets,
            extensions,
        } = node;
        sanitize_entries(&addrs, &sockets)?;
        let mut node = ContactInfo {
            pubkey,
            wallclock,
            outset,
            shred_version,
            version,
            addrs,
            sockets,
            extensions: tlv::parse(&extensions),
            cache: EMPTY_SOCKET_ADDR_CACHE,
        };
        // Populate node.cache.
        // Only sanitized socket-addrs can be cached!
        let mut port = 0u16;
        for &SocketEntry { key, index, offset } in &node.sockets {
            port += offset;
            let Some(entry) = node.cache.get_mut(usize::from(key)) else {
                continue;
            };
            let Some(&addr) = node.addrs.get(usize::from(index)) else {
                continue;
            };
            let socket = SocketAddr::new(addr, port);
            if sanitize_socket(&socket).is_ok() {
                *entry = socket;
            }
        }
        Ok(node)
    }
}

impl Sanitize for ContactInfo {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        if self.wallclock >= MAX_WALLCLOCK {
            return Err(SanitizeError::ValueOutOfBounds);
        }
        Ok(())
    }
}

pub(crate) fn sanitize_socket(socket: &SocketAddr) -> Result<(), Error> {
    if socket.port() == 0u16 {
        return Err(Error::InvalidPort(socket.port()));
    }
    let addr = socket.ip();
    if addr.is_unspecified() {
        return Err(Error::UnspecifiedIpAddr(addr));
    }
    if addr.is_multicast() {
        return Err(Error::MulticastIpAddr(addr));
    }
    Ok(())
}

// Sanitizes deserialized IpAddr and socket entries.
fn sanitize_entries(addrs: &[IpAddr], sockets: &[SocketEntry]) -> Result<(), Error> {
    // Verify that all IP addresses are unique.
    {
        let mut seen = HashSet::with_capacity(addrs.len());
        for addr in addrs {
            if !seen.insert(addr) {
                return Err(Error::DuplicateIpAddr(*addr));
            }
        }
    }
    // Verify that all socket entries have unique key.
    {
        let mut mask = [0u64; 4]; // 256-bit bitmask.
        for &SocketEntry { key, .. } in sockets {
            let mask = &mut mask[usize::from(key / 64u8)];
            let bit = 1u64 << (key % 64u8);
            if (*mask & bit) != 0u64 {
                return Err(Error::DuplicateSocket(key));
            }
            *mask |= bit;
        }
    }
    // Verify that all socket entries reference a valid IP address, and
    // that all IP addresses are referenced in the sockets.
    {
        let num_addrs = addrs.len();
        let mut hits = vec![false; num_addrs];
        for &SocketEntry { index, .. } in sockets {
            *hits
                .get_mut(usize::from(index))
                .ok_or(Error::InvalidIpAddrIndex { index, num_addrs })? = true;
        }
        if let Some(index) = hits.into_iter().position(|hit| !hit) {
            return Err(Error::UnusedIpAddr(addrs[index]));
        }
    }
    // Verify that port offsets don't overflow.
    if sockets
        .iter()
        .try_fold(0u16, |offset, entry| offset.checked_add(entry.offset))
        .is_none()
    {
        return Err(Error::PortOffsetsOverflow);
    }
    Ok(())
}

#[cfg(all(test, feature = "frozen-abi"))]
impl solana_frozen_abi::abi_example::AbiExample for ContactInfo {
    fn example() -> Self {
        Self {
            pubkey: Pubkey::example(),
            wallclock: u64::example(),
            outset: u64::example(),
            shred_version: u16::example(),
            version: solana_version::Version::example(),
            addrs: Vec::<IpAddr>::example(),
            sockets: Vec::<SocketEntry>::example(),
            extensions: vec![],
            cache: EMPTY_SOCKET_ADDR_CACHE,
        }
    }
}

#[macro_export]
macro_rules! socketaddr {
    ($ip:expr, $port:expr) => {
        std::net::SocketAddr::from((std::net::Ipv4Addr::from($ip), $port))
    };
    ($str:expr) => {{
        $str.parse::<std::net::SocketAddr>().unwrap()
    }};
}

#[macro_export]
macro_rules! socketaddr_any {
    () => {
        socketaddr!(std::net::Ipv4Addr::UNSPECIFIED, 0)
    };
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::{seq::SliceRandom, Rng},
        solana_keypair::Keypair,
        solana_signer::Signer,
        std::{
            collections::{HashMap, HashSet},
            iter::repeat_with,
            net::{Ipv4Addr, Ipv6Addr},
            ops::Range,
            time::Duration,
        },
    };

    fn new_rand_addr<R: Rng>(rng: &mut R) -> IpAddr {
        if rng.gen() {
            let addr = Ipv4Addr::new(rng.gen(), rng.gen(), rng.gen(), rng.gen());
            IpAddr::V4(addr)
        } else {
            let addr = Ipv6Addr::new(
                rng.gen(),
                rng.gen(),
                rng.gen(),
                rng.gen(),
                rng.gen(),
                rng.gen(),
                rng.gen(),
                rng.gen(),
            );
            IpAddr::V6(addr)
        }
    }

    fn new_rand_port<R: Rng>(rng: &mut R) -> u16 {
        let port = rng.gen::<u16>();
        let bits = u16::BITS - port.leading_zeros();
        let shift = rng.gen_range(0u32..bits + 1u32);
        port.checked_shr(shift).unwrap_or_default()
    }

    fn new_rand_socket<R: Rng>(rng: &mut R) -> SocketAddr {
        SocketAddr::new(new_rand_addr(rng), new_rand_port(rng))
    }

    #[test]
    fn test_new_gossip_entry_point() {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 10));
        let ci = ContactInfo::new_gossip_entry_point(&addr);
        assert_eq!(ci.gossip().unwrap(), addr);
        assert_matches!(ci.rpc(), None);
        assert_matches!(ci.rpc_pubsub(), None);
        assert_matches!(ci.serve_repair(Protocol::QUIC), None);
        assert_matches!(ci.serve_repair(Protocol::UDP), None);
        assert_matches!(ci.tpu(Protocol::QUIC), None);
        assert_matches!(ci.tpu(Protocol::UDP), None);
        assert_matches!(ci.tpu_forwards(Protocol::QUIC), None);
        assert_matches!(ci.tpu_forwards(Protocol::UDP), None);
        assert_matches!(ci.tpu_vote(Protocol::UDP), None);
        assert_matches!(ci.tpu_vote(Protocol::QUIC), None);
        assert_matches!(ci.tvu(Protocol::QUIC), None);
        assert_matches!(ci.tvu(Protocol::UDP), None);
        assert_matches!(ci.alpenglow(), None);
    }

    #[test]
    fn test_sanitize_entries() {
        let mut rng = rand::thread_rng();
        let addrs: Vec<IpAddr> = repeat_with(|| new_rand_addr(&mut rng)).take(5).collect();
        let mut keys: Vec<u8> = (0u8..=u8::MAX).collect();
        keys.shuffle(&mut rng);
        // Duplicate IP addresses.
        {
            let addrs = [addrs[0], addrs[1], addrs[2], addrs[0], addrs[3]];
            assert_matches!(
                sanitize_entries(&addrs, /*sockets:*/ &[]),
                Err(Error::DuplicateIpAddr(_))
            );
        }
        // Duplicate socket keys.
        {
            let keys = [0u8, 1, 5, 1, 3];
            let (index, offset) = (0u8, 0u16);
            let sockets: Vec<_> = keys
                .iter()
                .map(|&key| SocketEntry { key, index, offset })
                .collect();
            assert_matches!(
                sanitize_entries(/*addrs:*/ &[], &sockets),
                Err(Error::DuplicateSocket(_))
            );
        }
        // Invalid IP address index.
        {
            let offset = 0u16;
            let sockets: Vec<_> = [1u8, 2, 1, 3, 5, 3]
                .into_iter()
                .zip(&keys)
                .map(|(index, &key)| SocketEntry { key, index, offset })
                .collect();
            assert_matches!(
                sanitize_entries(&addrs, &sockets),
                Err(Error::InvalidIpAddrIndex { .. })
            );
        }
        // Unused IP address.
        {
            let sockets: Vec<_> = (0..4u8)
                .map(|key| SocketEntry {
                    key,
                    index: key,
                    offset: 0u16,
                })
                .collect();
            assert_matches!(
                sanitize_entries(&addrs, &sockets),
                Err(Error::UnusedIpAddr(_))
            );
        }
        // Port offsets overflow.
        {
            let sockets: Vec<_> = keys
                .iter()
                .map(|&key| SocketEntry {
                    key,
                    index: rng.gen_range(0u8..addrs.len() as u8),
                    offset: rng.gen_range(0u16..u16::MAX / 64),
                })
                .collect();
            assert_matches!(
                sanitize_entries(&addrs, &sockets),
                Err(Error::PortOffsetsOverflow)
            );
        }
        {
            let sockets: Vec<_> = keys
                .iter()
                .map(|&key| SocketEntry {
                    key,
                    index: rng.gen_range(0u8..addrs.len() as u8),
                    offset: rng.gen_range(0u16..u16::MAX / 256),
                })
                .collect();
            assert_matches!(sanitize_entries(&addrs, &sockets), Ok(()));
        }
    }

    #[test]
    fn test_round_trip() {
        const KEYS: Range<u8> = 0u8..16u8;
        let mut rng = rand::thread_rng();
        let addrs: Vec<IpAddr> = repeat_with(|| new_rand_addr(&mut rng)).take(8).collect();
        let mut node = ContactInfo {
            pubkey: Pubkey::new_unique(),
            wallclock: rng.gen(),
            outset: rng.gen(),
            shred_version: rng.gen(),
            version: solana_version::Version::default(),
            addrs: Vec::default(),
            sockets: Vec::default(),
            extensions: Vec::default(),
            cache: EMPTY_SOCKET_ADDR_CACHE,
        };
        let mut sockets = HashMap::<u8, SocketAddr>::new();
        for _ in 0..1 << 14 {
            let addr = addrs.choose(&mut rng).unwrap();
            let socket = SocketAddr::new(*addr, new_rand_port(&mut rng));
            let key = rng.gen_range(KEYS.start..KEYS.end);
            if sanitize_socket(&socket).is_ok() {
                sockets.insert(key, socket);
                assert_matches!(node.set_socket(key, socket), Ok(()));
                assert_matches!(sanitize_entries(&node.addrs, &node.sockets), Ok(()));
            } else {
                assert_matches!(node.set_socket(key, socket), Err(_));
            }
            for key in KEYS.clone() {
                let socket = sockets.get(&key);
                assert_eq!(node.get_socket(key).ok().as_ref(), socket);
                if usize::from(key) < SOCKET_CACHE_SIZE {
                    assert_eq!(
                        node.cache[usize::from(key)],
                        socket
                            .filter(|socket| sanitize_socket(socket).is_ok())
                            .copied()
                            .unwrap_or(SOCKET_ADDR_UNSPECIFIED),
                    );
                }
            }
            assert_eq!(node.gossip().as_ref(), sockets.get(&SOCKET_TAG_GOSSIP));
            assert_eq!(node.rpc().as_ref(), sockets.get(&SOCKET_TAG_RPC));
            assert_eq!(
                node.alpenglow().as_ref(),
                sockets.get(&SOCKET_TAG_ALPENGLOW)
            );
            assert_eq!(
                node.rpc_pubsub().as_ref(),
                sockets.get(&SOCKET_TAG_RPC_PUBSUB)
            );
            assert_eq!(
                node.serve_repair(Protocol::UDP).as_ref(),
                sockets.get(&SOCKET_TAG_SERVE_REPAIR)
            );
            assert_eq!(
                node.serve_repair(Protocol::QUIC).as_ref(),
                sockets.get(&SOCKET_TAG_SERVE_REPAIR_QUIC)
            );
            assert_eq!(
                node.tpu(Protocol::UDP).as_ref(),
                sockets.get(&SOCKET_TAG_TPU)
            );
            assert_eq!(
                node.tpu(Protocol::QUIC).as_ref(),
                sockets.get(&SOCKET_TAG_TPU_QUIC)
            );
            assert_eq!(
                node.tpu_forwards(Protocol::UDP).as_ref(),
                sockets.get(&SOCKET_TAG_TPU_FORWARDS)
            );
            assert_eq!(
                node.tpu_forwards(Protocol::QUIC).as_ref(),
                sockets.get(&SOCKET_TAG_TPU_FORWARDS_QUIC)
            );
            assert_eq!(
                node.tpu_vote(Protocol::UDP).as_ref(),
                sockets.get(&SOCKET_TAG_TPU_VOTE)
            );
            assert_eq!(
                node.tpu_vote(Protocol::QUIC).as_ref(),
                sockets.get(&SOCKET_TAG_TPU_VOTE_QUIC)
            );
            assert_eq!(
                node.tvu(Protocol::UDP).as_ref(),
                sockets.get(&SOCKET_TAG_TVU)
            );
            assert_eq!(
                node.tvu(Protocol::QUIC).as_ref(),
                sockets.get(&SOCKET_TAG_TVU_QUIC)
            );
            // Assert that all IP addresses are unique.
            assert_eq!(
                node.addrs.len(),
                node.addrs
                    .iter()
                    .copied()
                    .collect::<HashSet<IpAddr>>()
                    .len()
            );
            // Assert that all sockets have unique key.
            assert_eq!(
                node.sockets.len(),
                node.sockets
                    .iter()
                    .map(|entry| entry.key)
                    .collect::<HashSet<u8>>()
                    .len()
            );
            // Assert that only mapped addresses are stored.
            assert_eq!(
                node.addrs.iter().copied().collect::<HashSet<_>>(),
                sockets.values().map(SocketAddr::ip).collect::<HashSet<_>>(),
            );
            // Assert that all sockets reference a valid IP address.
            assert!(node
                .sockets
                .iter()
                .map(|entry| node.addrs.get(usize::from(entry.index)))
                .all(|addr| addr.is_some()));
            // Assert that port offsets don't overflow.
            assert!(u16::try_from(
                node.sockets
                    .iter()
                    .map(|entry| u64::from(entry.offset))
                    .sum::<u64>()
            )
            .is_ok());
            // Assert that serde round trips.
            let bytes = bincode::serialize(&node).unwrap();
            let other: ContactInfo = bincode::deserialize(&bytes).unwrap();
            assert_eq!(node, other);
        }
    }

    #[test]
    fn test_set_and_remove_alpenglow() {
        let mut rng = rand::thread_rng();
        let mut node = ContactInfo::new(
            Keypair::new().pubkey(),
            rng.gen(), // wallclock
            rng.gen(), // shred_version
        );
        let socket = repeat_with(|| new_rand_socket(&mut rng))
            .find(|socket| matches!(sanitize_socket(socket), Ok(())))
            .unwrap();
        node.set_alpenglow(socket).unwrap();
        assert_eq!(node.alpenglow().unwrap(), socket);
        node.remove_alpenglow();
        assert_matches!(node.alpenglow(), None);
    }

    #[test]
    fn test_check_duplicate() {
        let mut rng = rand::thread_rng();
        let mut node = ContactInfo::new(
            Keypair::new().pubkey(),
            rng.gen(), // wallclock
            rng.gen(), // shred_version
        );
        // Same contact-info is not a duplicate instance.
        {
            let other = node.clone();
            assert!(!node.check_duplicate(&other));
            assert!(!other.check_duplicate(&node));
            assert_eq!(node.overrides(&other), None);
            assert_eq!(other.overrides(&node), None);
        }
        // Updated socket address is not a duplicate instance.
        {
            let mut other = node.clone();
            while other.set_gossip(new_rand_socket(&mut rng)).is_err() {}
            while other
                .set_serve_repair(Protocol::UDP, new_rand_socket(&mut rng))
                .is_err()
            {}
            assert!(!node.check_duplicate(&other));
            assert!(!other.check_duplicate(&node));
            assert_eq!(node.overrides(&other), None);
            assert_eq!(other.overrides(&node), None);
            other.remove_serve_repair();
            assert!(!node.check_duplicate(&other));
            assert!(!other.check_duplicate(&node));
            assert_eq!(node.overrides(&other), None);
            assert_eq!(other.overrides(&node), None);
        }
        // Updated wallclock is not a duplicate instance.
        {
            let other = node.clone();
            node.set_wallclock(rng.gen());
            assert!(!node.check_duplicate(&other));
            assert!(!other.check_duplicate(&node));
            assert_eq!(
                node.overrides(&other),
                Some(other.wallclock < node.wallclock)
            );
            assert_eq!(
                other.overrides(&node),
                Some(node.wallclock < other.wallclock)
            );
        }
        // Different pubkey is not a duplicate instance.
        {
            let other = ContactInfo::new(
                Keypair::new().pubkey(),
                rng.gen(), // wallclock
                rng.gen(), // shred_version
            );
            assert!(!node.check_duplicate(&other));
            assert!(!other.check_duplicate(&node));
            assert_eq!(node.overrides(&other), None);
            assert_eq!(other.overrides(&node), None);

            // Need to sleep here so that get_node_outset
            // returns a larger value.
            std::thread::sleep(Duration::from_millis(1));

            node.hot_swap_pubkey(*other.pubkey());
            assert!(node.outset > other.outset);
            assert!(!node.check_duplicate(&other));
            assert!(other.check_duplicate(&node));
            assert_eq!(node.overrides(&other), Some(true));
            assert_eq!(other.overrides(&node), Some(false));
        }
        // Same pubkey, more recent outset timestamp is a duplicate instance.
        {
            std::thread::sleep(Duration::from_millis(1));
            let other = ContactInfo::new(
                node.pubkey,
                rng.gen(), // wallclock
                rng.gen(), // shred_version
            );
            assert!(node.outset < other.outset);
            assert!(node.check_duplicate(&other));
            assert!(!other.check_duplicate(&node));
            assert_eq!(node.overrides(&other), Some(false));
            assert_eq!(other.overrides(&node), Some(true));
            node.set_wallclock(other.wallclock);
            assert!(node.outset < other.outset);
            assert!(node.check_duplicate(&other));
            assert!(!other.check_duplicate(&node));
            assert_eq!(node.overrides(&other), Some(false));
            assert_eq!(other.overrides(&node), Some(true));
        }
    }
}
