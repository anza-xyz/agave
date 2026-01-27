#![allow(clippy::arithmetic_side_effects)]

use {
    libc::{
        nlattr, nlmsgerr, nlmsghdr, recv, send, setsockopt, sockaddr_nl, socket, AF_INET, AF_INET6,
        AF_NETLINK, NDA_DST, NDA_LLADDR, NETLINK_EXT_ACK, NETLINK_ROUTE, NLA_ALIGNTO,
        NLA_TYPE_MASK, NLMSG_DONE, NLMSG_ERROR, NLM_F_DUMP, NLM_F_MULTI, NLM_F_REQUEST, RTA_DST,
        RTA_GATEWAY, RTA_IIF, RTA_OIF, RTA_PREFSRC, RTA_PRIORITY, RTA_TABLE, RTM_GETNEIGH,
        RTM_GETROUTE, RTM_NEWNEIGH, RTM_NEWROUTE, RT_TABLE_MAIN, SOCK_RAW, SOL_NETLINK, SOL_SOCKET,
        SO_RCVBUF,
    },
    std::{
        collections::HashMap,
        io, mem,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        ptr, slice,
    },
    thiserror::Error,
};

const NETLINK_RCVBUF_SIZE: i32 = 1 << 16;
const NLA_HDR_LEN: usize = align_to(mem::size_of::<nlattr>(), NLA_ALIGNTO as usize);

pub struct NetlinkSocket {
    sock: OwnedFd,
    _nl_pid: u32,
}

impl NetlinkSocket {
    fn open() -> Result<Self, io::Error> {
        // Safety: libc wrapper
        let sock = unsafe { socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE) };
        if sock < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `socket` returns a file descriptor.
        let sock = unsafe { OwnedFd::from_raw_fd(sock) };

        let enable = 1i32;
        // Safety: libc wrapper
        if unsafe {
            setsockopt(
                sock.as_raw_fd(),
                SOL_NETLINK,
                NETLINK_EXT_ACK,
                &enable as *const _ as *const _,
                mem::size_of::<i32>() as u32,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { sock, _nl_pid: 0 })
    }

    fn send(&self, msg: &[u8]) -> Result<(), io::Error> {
        if unsafe {
            send(
                self.sock.as_raw_fd(),
                msg.as_ptr() as *const _,
                msg.len(),
                0,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn recv(&self) -> Result<Vec<NetlinkMessage>, io::Error> {
        // The theoretical max size of a single netlink message (including header) is 4GiB.
        // See: https://elixir.bootlin.com/linux/v6.17.7/source/include/uapi/linux/netlink.h#L46
        // However, in the kernel, the netlink message size is set to a page size.
        // If the page size exceeds 8KiB, the netlink message size is capped to 8KiB
        // See: https://elixir.bootlin.com/linux/v6.17.7/source/include/linux/netlink.h#L267
        let mut buf = [0u8; 8 * 1024]; // 8 KiB
        let mut messages = Vec::new();
        let mut multipart = true;
        'out: while multipart {
            multipart = false;
            // Safety: libc wrapper
            let len = unsafe {
                recv(
                    self.sock.as_raw_fd(),
                    buf.as_mut_ptr() as *mut _,
                    buf.len(),
                    0,
                )
            };
            if len < 0 {
                return Err(io::Error::last_os_error());
            }
            if len == 0 {
                break;
            }

            let len = len as usize;
            let mut offset = 0;
            while offset < len {
                let message = NetlinkMessage::read(&buf[offset..])?;
                offset += align_to(message.header.nlmsg_len as usize, NLMSG_ALIGNTO as usize);
                multipart = message.header.nlmsg_flags & NLM_F_MULTI as u16 != 0;
                match message.header.nlmsg_type as i32 {
                    NLMSG_ERROR => {
                        let err = message.error.unwrap();
                        if err.error == 0 {
                            // this is an ACK
                            continue;
                        }
                        return Err(io::Error::from_raw_os_error(-err.error));
                    }
                    NLMSG_DONE => break 'out,
                    _ => messages.push(message),
                }
            }
        }

        Ok(messages)
    }

    /// Opens a listener socket for netlink updates
    /// NETLINK_ROUTE socket subscribed to `groups` bitmask
    pub fn bind(groups: u32) -> Result<Self, io::Error> {
        let sock = Self::open()?;

        // Subscribe to multicast groups
        let mut addr: sockaddr_nl = unsafe { mem::zeroed() };
        addr.nl_family = AF_NETLINK as u16;
        addr.nl_groups = groups;
        if unsafe {
            libc::bind(
                sock.as_raw_fd(),
                &addr as *const _ as *const _,
                mem::size_of::<sockaddr_nl>() as u32,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }

        unsafe {
            setsockopt(
                sock.as_raw_fd(),
                SOL_SOCKET,
                SO_RCVBUF,
                &NETLINK_RCVBUF_SIZE as *const _ as *const _,
                mem::size_of::<i32>() as u32,
            );
        }

        Ok(sock)
    }

    #[inline]
    pub fn as_raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

#[derive(Debug, Clone)]
pub struct NetlinkMessage {
    pub(crate) header: nlmsghdr,
    data: Vec<u8>,
    error: Option<nlmsgerr>,
}

impl NetlinkMessage {
    fn read(buf: &[u8]) -> Result<Self, io::Error> {
        if mem::size_of::<nlmsghdr>() > buf.len() {
            return Err(io::Error::other("buffer smaller than nlmsghdr"));
        }

        // Safety: nlmsghdr is POD so read is safe
        let header = unsafe { ptr::read_unaligned(buf.as_ptr() as *const nlmsghdr) };
        let msg_len = header.nlmsg_len as usize;
        if msg_len < mem::size_of::<nlmsghdr>() || msg_len > buf.len() {
            return Err(io::Error::other("invalid nlmsg_len"));
        }

        let data_offset = align_to(mem::size_of::<nlmsghdr>(), NLMSG_ALIGNTO as usize);
        if data_offset >= buf.len() {
            return Err(io::Error::other("need more data"));
        }

        let (data, error) = if header.nlmsg_type == NLMSG_ERROR as u16 {
            if data_offset + mem::size_of::<nlmsgerr>() > buf.len() {
                return Err(io::Error::other(
                    "NLMSG_ERROR but not enough space for nlmsgerr",
                ));
            }
            (
                Vec::new(),
                // Safety: nlmsgerr is POD so read is safe
                Some(unsafe {
                    ptr::read_unaligned(buf[data_offset..].as_ptr() as *const nlmsgerr)
                }),
            )
        } else {
            (buf[data_offset..msg_len].to_vec(), None)
        };

        Ok(Self {
            header,
            data,
            error,
        })
    }
}

const fn align_to(v: usize, align: usize) -> usize {
    (v + (align - 1)) & !(align - 1)
}

struct NlAttrsIterator<'a> {
    attrs: &'a [u8],
    offset: usize,
}

impl<'a> NlAttrsIterator<'a> {
    fn new(attrs: &'a [u8]) -> Self {
        Self { attrs, offset: 0 }
    }
}

impl<'a> Iterator for NlAttrsIterator<'a> {
    type Item = Result<NlAttr<'a>, NlAttrError>;

    fn next(&mut self) -> Option<Self::Item> {
        let buf = &self.attrs[self.offset..];
        if buf.is_empty() {
            return None;
        }

        if NLA_HDR_LEN > buf.len() {
            self.offset = buf.len();
            return Some(Err(NlAttrError::InvalidBufferLength {
                size: buf.len(),
                expected: NLA_HDR_LEN,
            }));
        }

        let attr = unsafe { ptr::read_unaligned(buf.as_ptr() as *const nlattr) };
        let len = attr.nla_len as usize;
        let align_len = align_to(len, NLA_ALIGNTO as usize);
        if len < NLA_HDR_LEN {
            return Some(Err(NlAttrError::InvalidHeaderLength(len)));
        }
        if align_len > buf.len() {
            return Some(Err(NlAttrError::InvalidBufferLength {
                size: buf.len(),
                expected: align_len,
            }));
        }

        let data = &buf[NLA_HDR_LEN..len];

        self.offset += align_len;
        Some(Ok(NlAttr { header: attr, data }))
    }
}

fn parse_attrs(buf: &[u8]) -> Result<HashMap<u16, NlAttr<'_>>, NlAttrError> {
    let mut attrs = HashMap::new();
    for attr in NlAttrsIterator::new(buf) {
        let attr = attr?;
        attrs.insert(attr.header.nla_type & NLA_TYPE_MASK as u16, attr);
    }
    Ok(attrs)
}

#[derive(Clone)]
struct NlAttr<'a> {
    header: nlattr,
    data: &'a [u8],
}

#[derive(Debug, Error, PartialEq, Eq)]
enum NlAttrError {
    #[error("invalid buffer size `{size}`, expected `{expected}`")]
    InvalidBufferLength { size: usize, expected: usize },

    #[error("invalid nlattr header length `{0}`")]
    InvalidHeaderLength(usize),
}

impl From<NlAttrError> for io::Error {
    fn from(e: NlAttrError) -> Self {
        Self::other(e)
    }
}

fn bytes_of<T>(val: &T) -> &[u8] {
    let size = mem::size_of::<T>();
    unsafe { slice::from_raw_parts(slice::from_ref(val).as_ptr().cast(), size) }
}

const NLMSG_ALIGNTO: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddress(pub [u8; 6]);

impl MacAddress {
    pub fn new(bytes: [u8; 6]) -> Self {
        MacAddress(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 6] {
        &self.0
    }
}

impl std::fmt::Display for MacAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

/// Represents an entry in the neighbor table (ARP/NDP cache)
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NeighborEntry {
    // IPv4 or IPv6 address
    pub destination: Option<IpAddr>,
    // MAC address
    pub lladdr: Option<MacAddress>,
    // Interface index
    pub ifindex: i32,
    // NUD_* state
    pub state: u16,
}

impl NeighborEntry {
    #[inline]
    pub fn key(&self) -> Option<(i32, Ipv4Addr)> {
        match self.destination {
            Some(IpAddr::V4(ip)) => Some((self.ifindex, ip)),
            _ => None,
        }
    }
}

#[repr(C)]
#[allow(non_camel_case_types)]
struct ndmsg {
    ndm_family: u8,
    _ndm_pad1: u8,
    _ndm_pad2: u16,
    ndm_ifindex: i32,
    ndm_state: u16,
    _ndm_flags: u8,
    _ndm_type: u8,
}

#[repr(C)]
struct NeighRequest {
    header: nlmsghdr,
    ndm: ndmsg,
}

/// fetch the kernel's neighbor table (ARP/NDP cache)
pub fn netlink_get_neighbors(
    if_index: Option<u32>,
    family: u8,
) -> Result<Vec<NeighborEntry>, io::Error> {
    let sock = NetlinkSocket::open()?;

    // Safety: NeighRequest is POD
    let mut req = unsafe { mem::zeroed::<NeighRequest>() };

    let nlmsg_len = mem::size_of::<nlmsghdr>() + mem::size_of::<ndmsg>();
    req.header = nlmsghdr {
        nlmsg_len: nlmsg_len as u32,
        nlmsg_flags: (NLM_F_REQUEST | NLM_F_DUMP) as u16,
        nlmsg_type: RTM_GETNEIGH,
        nlmsg_pid: 0,
        nlmsg_seq: 1,
    };

    req.ndm.ndm_family = family;
    if let Some(idx) = if_index {
        req.ndm.ndm_ifindex = idx as i32;
    }

    sock.send(&bytes_of(&req)[..req.header.nlmsg_len as usize])?;

    let mut neighbors = Vec::new();

    for msg in sock.recv()? {
        if msg.header.nlmsg_type != RTM_NEWNEIGH {
            continue;
        }

        if msg.data.len() < mem::size_of::<ndmsg>() {
            continue;
        }

        if let Some(neighbor) = parse_rtm_newneigh(&msg, if_index) {
            neighbors.push(neighbor);
        }
    }

    Ok(neighbors)
}

pub fn parse_rtm_newneigh(msg: &NetlinkMessage, if_index: Option<u32>) -> Option<NeighborEntry> {
    if msg.data.len() < mem::size_of::<ndmsg>() {
        return None;
    }
    let nd_msg = unsafe { ptr::read_unaligned(msg.data.as_ptr() as *const ndmsg) };
    if let Some(idx) = if_index {
        if nd_msg.ndm_ifindex != idx as i32 {
            return None;
        }
    }
    let Ok(attrs) = parse_attrs(&msg.data[mem::size_of::<ndmsg>()..]) else {
        return None;
    };
    let mut neighbor = NeighborEntry {
        destination: None,
        lladdr: None,
        ifindex: nd_msg.ndm_ifindex,
        state: nd_msg.ndm_state,
    };
    if let Some(dst_attr) = attrs.get(&NDA_DST) {
        neighbor.destination = parse_ip_address(dst_attr.data, nd_msg.ndm_family);
    }
    if let Some(lladdr_attr) = attrs.get(&NDA_LLADDR) {
        if lladdr_attr.data.len() >= 6 {
            let mut mac = [0u8; 6];
            mac.copy_from_slice(&lladdr_attr.data[0..6]);
            neighbor.lladdr = Some(MacAddress(mac));
        }
    }
    Some(neighbor)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    pub destination: Option<IpAddr>,
    pub gateway: Option<IpAddr>,
    pub pref_src: Option<IpAddr>,
    pub out_if_index: Option<i32>,
    pub in_if_index: Option<i32>,
    pub priority: Option<u32>,
    pub table: Option<u32>,
    pub protocol: u8,
    pub scope: u8,
    pub type_: u8,
    pub family: u8,
    pub dst_len: u8,
    pub flags: u32,
}

impl RouteEntry {
    #[inline]
    pub fn same_key(&self, other: &Self) -> bool {
        self.family == other.family
            && self.dst_len == other.dst_len
            && self.destination == other.destination
            && self.table == other.table
            && self.type_ == other.type_
    }
}

#[repr(C)]
#[allow(non_camel_case_types)]
struct rtmsg {
    rtm_family: u8,
    rtm_dst_len: u8,
    rtm_src_len: u8,
    rtm_tos: u8,
    rtm_table: u8,
    rtm_protocol: u8,
    rtm_scope: u8,
    rtm_type: u8,
    rtm_flags: u32,
}

#[repr(C)]
struct RouteRequest {
    header: nlmsghdr,
    rtm: rtmsg,
}

fn parse_ip_address(data: &[u8], family: u8) -> Option<IpAddr> {
    match family as i32 {
        AF_INET if data.len() == 4 => Some(IpAddr::V4(Ipv4Addr::new(
            data[0], data[1], data[2], data[3],
        ))),
        AF_INET6 if data.len() == 16 => {
            let mut segments = [0u16; 8];
            for i in 0..8 {
                segments[i] = ((data[i * 2] as u16) << 8) | (data[i * 2 + 1] as u16);
            }
            Some(IpAddr::V6(Ipv6Addr::from(segments)))
        }
        _ => None,
    }
}

pub fn netlink_get_routes(family: u8) -> Result<Vec<RouteEntry>, io::Error> {
    let sock = NetlinkSocket::open()?;

    // Safety: RouteRequest is POD
    let mut req = unsafe { mem::zeroed::<RouteRequest>() };

    let nlmsg_len = mem::size_of::<nlmsghdr>() + mem::size_of::<rtmsg>();
    req.header = nlmsghdr {
        nlmsg_len: nlmsg_len as u32,
        nlmsg_flags: (NLM_F_REQUEST | NLM_F_DUMP) as u16,
        nlmsg_type: RTM_GETROUTE,
        nlmsg_pid: 0,
        nlmsg_seq: 1,
    };

    req.rtm.rtm_family = family;
    req.rtm.rtm_table = RT_TABLE_MAIN;

    sock.send(&bytes_of(&req)[..req.header.nlmsg_len as usize])?;

    let mut routes = Vec::new();

    for msg in sock.recv()? {
        if msg.header.nlmsg_type != RTM_NEWROUTE {
            continue;
        }

        if msg.data.len() < mem::size_of::<rtmsg>() {
            continue;
        }

        if let Some(route) = parse_rtm_newroute(&msg) {
            routes.push(route);
        }
    }

    Ok(routes)
}

pub fn parse_rtm_newroute(msg: &NetlinkMessage) -> Option<RouteEntry> {
    if msg.data.len() < mem::size_of::<rtmsg>() {
        return None;
    }
    let rt_msg = unsafe { ptr::read_unaligned(msg.data.as_ptr() as *const rtmsg) };
    let Ok(attrs) = parse_attrs(&msg.data[mem::size_of::<rtmsg>()..]) else {
        return None;
    };
    let mut route = RouteEntry {
        destination: None,
        gateway: None,
        pref_src: None,
        out_if_index: None,
        in_if_index: None,
        priority: None,
        table: None,
        protocol: rt_msg.rtm_protocol,
        scope: rt_msg.rtm_scope,
        type_: rt_msg.rtm_type,
        family: rt_msg.rtm_family,
        dst_len: rt_msg.rtm_dst_len,
        flags: rt_msg.rtm_flags,
    };
    if let Some(dst_attr) = attrs.get(&RTA_DST) {
        route.destination = parse_ip_address(dst_attr.data, rt_msg.rtm_family);
    }
    if let Some(gateway_attr) = attrs.get(&RTA_GATEWAY) {
        route.gateway = parse_ip_address(gateway_attr.data, rt_msg.rtm_family);
    }

    let u32_from_ne_bytes = |data: &[u8]| -> Option<u32> {
        data.get(..4)
            .map(|data| u32::from_ne_bytes([data[0], data[1], data[2], data[3]]))
    };

    if let Some(oif_attr) = attrs.get(&RTA_OIF) {
        route.out_if_index = u32_from_ne_bytes(oif_attr.data).map(|i| i as i32);
    }
    if let Some(iif_attr) = attrs.get(&RTA_IIF) {
        route.in_if_index = u32_from_ne_bytes(iif_attr.data).map(|i| i as i32);
    }
    if let Some(priority_attr) = attrs.get(&RTA_PRIORITY) {
        route.priority = u32_from_ne_bytes(priority_attr.data);
    }
    if let Some(table_attr) = attrs.get(&RTA_TABLE) {
        route.table = u32_from_ne_bytes(table_attr.data);
    }
    if let Some(prefsrc_attr) = attrs.get(&RTA_PREFSRC) {
        route.pref_src = parse_ip_address(prefsrc_attr.data, rt_msg.rtm_family);
    }
    Some(route)
}

pub fn netlink_get_default_gateway(family: u8) -> Result<Option<RouteEntry>, io::Error> {
    let routes = netlink_get_routes(family)?;

    for route in routes {
        let is_default_destination = match (route.destination, family as i32) {
            (None, _) => true,
            // 0.0.0.0
            (Some(IpAddr::V4(addr)), AF_INET) => addr.is_unspecified() && route.dst_len == 0,
            // ::/0
            (Some(IpAddr::V6(addr)), AF_INET6) => addr.is_unspecified() && route.dst_len == 0,
            _ => false,
        };

        if is_default_destination && route.gateway.is_some() {
            return Ok(Some(route));
        }
    }

    Ok(None)
}
