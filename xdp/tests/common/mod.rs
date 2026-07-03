#![cfg(target_os = "linux")]
#![allow(dead_code)]

pub use aya::test_helpers::NetNsGuard;
use {
    agave_xdp::netlink::MacAddress,
    nix::sys::socket::{
        recv, sendto, socket, AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol,
        SockType,
    },
    std::{
        ffi::CString,
        io, mem,
        net::Ipv4Addr,
        os::fd::AsRawFd,
        ptr, slice, thread,
        time::{Duration, Instant},
    },
};

pub const LEFT_IFACE: &str = "axdp0";
pub const RIGHT_IFACE: &str = "axdp1";
pub const GRE_IFACE: &str = "gxdp0";

const NLA_HDR_LEN: usize = mem::size_of::<libc::nlattr>();
const NLMSG_ALIGNTO: usize = 4;
const SIOCADDTUNNEL: libc::c_int = 0x89F1;
const IPPROTO_GRE: u8 = 47;
const IP_DF: u16 = 0x4000;

// Nested attributes from include/uapi/linux/veth.h.
const VETH_INFO_PEER: u16 = 1;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TestLinks {
    pub left_if_index: u32,
    pub right_if_index: u32,
    pub left_ip: std::net::Ipv4Addr,
    pub right_ip: std::net::Ipv4Addr,
    pub left_mac: MacAddress,
    pub right_mac: MacAddress,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TestGreTunnel {
    pub if_index: u32,
    pub local_ip: std::net::Ipv4Addr,
    pub remote_ip: std::net::Ipv4Addr,
    pub overlay_ip: std::net::Ipv4Addr,
}

#[repr(C)]
struct IfInfoMsg {
    ifi_family: u8,
    ifi_pad: u8,
    ifi_type: u16,
    ifi_index: i32,
    ifi_flags: u32,
    ifi_change: u32,
}

#[repr(C)]
struct IfAddrMsg {
    ifa_family: u8,
    ifa_prefixlen: u8,
    ifa_flags: u8,
    ifa_scope: u8,
    ifa_index: u32,
}

#[repr(C)]
struct RtMsg {
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
struct NdMsg {
    ndm_family: u8,
    ndm_pad1: u8,
    ndm_pad2: u16,
    ndm_ifindex: i32,
    ndm_state: u16,
    ndm_flags: u8,
    ndm_type: u8,
}

#[repr(C)]
struct IpHdr {
    version_ihl: u8,
    tos: u8,
    tot_len: u16,
    id: u16,
    frag_off: u16,
    ttl: u8,
    protocol: u8,
    check: u16,
    saddr: u32,
    daddr: u32,
}

#[repr(C)]
struct IpTunnelParm {
    name: [libc::c_char; libc::IFNAMSIZ],
    link: i32,
    i_flags: u16,
    o_flags: u16,
    i_key: u32,
    o_key: u32,
    iph: IpHdr,
}

pub fn setup_veth_pair() -> TestLinks {
    let left_ip = Ipv4Addr::new(10, 0, 0, 1);
    let right_ip = Ipv4Addr::new(10, 0, 0, 2);
    let left_mac = MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01]);
    let right_mac = MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x02]);

    create_veth_pair(LEFT_IFACE, RIGHT_IFACE);
    set_link_mac(LEFT_IFACE, left_mac);
    set_link_mac(RIGHT_IFACE, right_mac);
    add_ipv4_addr(left_ip, 24, LEFT_IFACE);
    add_ipv4_addr(right_ip, 24, RIGHT_IFACE);
    set_link_up(LEFT_IFACE);
    set_link_up(RIGHT_IFACE);

    TestLinks {
        left_if_index: if_index(LEFT_IFACE),
        right_if_index: if_index(RIGHT_IFACE),
        left_ip,
        right_ip,
        left_mac,
        right_mac,
    }
}

pub fn setup_gre_tunnel(underlay: &TestLinks) -> TestGreTunnel {
    let overlay_ip = Ipv4Addr::new(192, 0, 2, 1);

    create_gre_tunnel(GRE_IFACE, underlay.left_ip, underlay.right_ip, 64);
    add_ipv4_addr(overlay_ip, 32, GRE_IFACE);
    set_link_up(GRE_IFACE);

    TestGreTunnel {
        if_index: if_index(GRE_IFACE),
        local_ip: underlay.left_ip,
        remote_ip: underlay.right_ip,
        overlay_ip,
    }
}

pub fn add_route(destination: &str, via: Ipv4Addr, dev: &str) {
    replace_route(destination, Some(via), dev, None);
}

pub fn add_route_to_dev(destination: &str, dev: &str) {
    replace_route(destination, None, dev, None);
}

pub fn add_route_to_dev_with_src(destination: &str, dev: &str, src: Ipv4Addr) {
    replace_route(destination, None, dev, Some(src));
}

pub fn delete_link(dev: &str) {
    let link = IfInfoMsg {
        ifi_family: libc::AF_UNSPEC as u8,
        ifi_pad: 0,
        ifi_type: 0,
        ifi_index: if_index(dev) as i32,
        ifi_flags: 0,
        ifi_change: 0,
    };
    send_rtnl_request(
        "delete link",
        libc::RTM_DELLINK,
        (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16,
        |request| request.extend_from_slice(bytes_of(&link)),
    );
}

#[allow(dead_code)]
pub fn delete_route(destination: &str) {
    let (destination, prefix_len) = parse_ipv4_prefix(destination);
    let route = route_msg(prefix_len, libc::RT_SCOPE_UNIVERSE);
    send_rtnl_request(
        "delete route",
        libc::RTM_DELROUTE,
        (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16,
        |request| {
            request.extend_from_slice(bytes_of(&route));
            push_attr(request, libc::RTA_DST, &destination.octets());
        },
    );
}

pub fn replace_neighbor(ip: Ipv4Addr, mac: MacAddress, dev: &str) {
    let neighbor = NdMsg {
        ndm_family: libc::AF_INET as u8,
        ndm_pad1: 0,
        ndm_pad2: 0,
        ndm_ifindex: if_index(dev) as i32,
        ndm_state: libc::NUD_PERMANENT,
        ndm_flags: 0,
        ndm_type: 0,
    };
    send_rtnl_request(
        "replace neighbor",
        libc::RTM_NEWNEIGH,
        (libc::NLM_F_REQUEST | libc::NLM_F_ACK | libc::NLM_F_CREATE | libc::NLM_F_REPLACE) as u16,
        |request| {
            request.extend_from_slice(bytes_of(&neighbor));
            push_attr(request, libc::NDA_DST, &ip.octets());
            push_attr(request, libc::NDA_LLADDR, mac.as_bytes());
        },
    );
}

pub fn delete_neighbor(ip: Ipv4Addr, dev: &str) {
    let neighbor = NdMsg {
        ndm_family: libc::AF_INET as u8,
        ndm_pad1: 0,
        ndm_pad2: 0,
        ndm_ifindex: if_index(dev) as i32,
        ndm_state: 0,
        ndm_flags: 0,
        ndm_type: 0,
    };
    send_rtnl_request(
        "delete neighbor",
        libc::RTM_DELNEIGH,
        (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16,
        |request| {
            request.extend_from_slice(bytes_of(&neighbor));
            push_attr(request, libc::NDA_DST, &ip.octets());
        },
    );
}

#[allow(dead_code)]
pub fn wait_until<T, F>(description: &str, timeout: Duration, mut predicate: F) -> T
where
    F: FnMut() -> Option<T>,
{
    let start = Instant::now();
    loop {
        if let Some(value) = predicate() {
            return value;
        }

        if start.elapsed() >= timeout {
            panic!("timed out waiting for {description}");
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn create_veth_pair(left_name: &str, right_name: &str) {
    let link = IfInfoMsg {
        ifi_family: libc::AF_UNSPEC as u8,
        ifi_pad: 0,
        ifi_type: 0,
        ifi_index: 0,
        ifi_flags: 0,
        ifi_change: 0,
    };
    let peer = IfInfoMsg {
        ifi_family: libc::AF_UNSPEC as u8,
        ifi_pad: 0,
        ifi_type: 0,
        ifi_index: 0,
        ifi_flags: 0,
        ifi_change: 0,
    };
    let left_name = ifname_attr(left_name);
    let right_name = ifname_attr(right_name);

    send_rtnl_request(
        "create veth pair",
        libc::RTM_NEWLINK,
        (libc::NLM_F_REQUEST | libc::NLM_F_ACK | libc::NLM_F_CREATE | libc::NLM_F_EXCL) as u16,
        |request| {
            request.extend_from_slice(bytes_of(&link));
            push_attr(request, libc::IFLA_IFNAME, &left_name);
            push_nested_attr(request, libc::IFLA_LINKINFO, |linkinfo| {
                push_attr(linkinfo, libc::IFLA_INFO_KIND, b"veth\0");
                push_nested_attr(linkinfo, libc::IFLA_INFO_DATA, |info_data| {
                    push_nested_attr(info_data, VETH_INFO_PEER, |peer_attr| {
                        peer_attr.extend_from_slice(bytes_of(&peer));
                        push_attr(peer_attr, libc::IFLA_IFNAME, &right_name);
                    });
                });
            });
        },
    );
}

fn create_gre_tunnel(name: &str, local: Ipv4Addr, remote: Ipv4Addr, ttl: u8) {
    let sock = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::empty(),
        None::<SockProtocol>,
    )
    .map_err(io::Error::from)
    .unwrap_or_else(|err| panic!("create GRE tunnel control socket failed: {err}"));

    let mut parm = IpTunnelParm {
        name: [0; libc::IFNAMSIZ],
        link: 0,
        i_flags: 0,
        o_flags: 0,
        i_key: 0,
        o_key: 0,
        iph: IpHdr {
            version_ihl: 4u8.wrapping_shl(4) | 5,
            tos: 0,
            tot_len: 0,
            id: 0,
            frag_off: IP_DF.to_be(),
            ttl,
            protocol: IPPROTO_GRE,
            check: 0,
            saddr: u32::from_be_bytes(local.octets()).to_be(),
            daddr: u32::from_be_bytes(remote.octets()).to_be(),
        },
    };
    write_fixed_ifname(&mut parm.name, name);

    let mut request = libc::ifreq {
        ifr_name: [0; libc::IFNAMSIZ],
        ifr_ifru: libc::__c_anonymous_ifr_ifru {
            ifru_data: ptr::addr_of_mut!(parm).cast(),
        },
    };
    write_fixed_ifname(&mut request.ifr_name, "gre0");

    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), SIOCADDTUNNEL, ptr::addr_of_mut!(request)) };
    if rc != 0 {
        panic!("create GRE tunnel failed: {}", io::Error::last_os_error());
    }
}

fn set_link_mac(dev: &str, mac: MacAddress) {
    let link = IfInfoMsg {
        ifi_family: libc::AF_UNSPEC as u8,
        ifi_pad: 0,
        ifi_type: 0,
        ifi_index: if_index(dev) as i32,
        ifi_flags: 0,
        ifi_change: 0,
    };
    send_rtnl_request(
        "set link MAC address",
        libc::RTM_NEWLINK,
        (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16,
        |request| {
            request.extend_from_slice(bytes_of(&link));
            push_attr(request, libc::IFLA_ADDRESS, mac.as_bytes());
        },
    );
}

fn set_link_up(dev: &str) {
    let link = IfInfoMsg {
        ifi_family: libc::AF_UNSPEC as u8,
        ifi_pad: 0,
        ifi_type: 0,
        ifi_index: if_index(dev) as i32,
        ifi_flags: libc::IFF_UP as u32,
        ifi_change: libc::IFF_UP as u32,
    };
    send_rtnl_request(
        "set link up",
        libc::RTM_NEWLINK,
        (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16,
        |request| request.extend_from_slice(bytes_of(&link)),
    );
}

fn add_ipv4_addr(ip: Ipv4Addr, prefix_len: u8, dev: &str) {
    assert!(prefix_len <= 32, "invalid IPv4 prefix length {prefix_len}");
    let addr = IfAddrMsg {
        ifa_family: libc::AF_INET as u8,
        ifa_prefixlen: prefix_len,
        ifa_flags: 0,
        ifa_scope: libc::RT_SCOPE_UNIVERSE,
        ifa_index: if_index(dev),
    };
    send_rtnl_request(
        "add IPv4 address",
        libc::RTM_NEWADDR,
        (libc::NLM_F_REQUEST | libc::NLM_F_ACK | libc::NLM_F_CREATE | libc::NLM_F_EXCL) as u16,
        |request| {
            request.extend_from_slice(bytes_of(&addr));
            push_attr(request, libc::IFA_LOCAL, &ip.octets());
            push_attr(request, libc::IFA_ADDRESS, &ip.octets());
        },
    );
}

fn replace_route(destination: &str, gateway: Option<Ipv4Addr>, dev: &str, src: Option<Ipv4Addr>) {
    let (destination, prefix_len) = parse_ipv4_prefix(destination);
    let scope = if gateway.is_some() {
        libc::RT_SCOPE_UNIVERSE
    } else {
        libc::RT_SCOPE_LINK
    };
    let route = route_msg(prefix_len, scope);
    let if_index = if_index(dev);

    send_rtnl_request(
        "replace route",
        libc::RTM_NEWROUTE,
        (libc::NLM_F_REQUEST | libc::NLM_F_ACK | libc::NLM_F_CREATE | libc::NLM_F_REPLACE) as u16,
        |request| {
            request.extend_from_slice(bytes_of(&route));
            push_attr(request, libc::RTA_DST, &destination.octets());
            push_attr(request, libc::RTA_OIF, bytes_of(&if_index));
            if let Some(gateway) = gateway {
                push_attr(request, libc::RTA_GATEWAY, &gateway.octets());
            }
            if let Some(src) = src {
                push_attr(request, libc::RTA_PREFSRC, &src.octets());
            }
        },
    );
}

fn route_msg(prefix_len: u8, scope: u8) -> RtMsg {
    RtMsg {
        rtm_family: libc::AF_INET as u8,
        rtm_dst_len: prefix_len,
        rtm_src_len: 0,
        rtm_tos: 0,
        rtm_table: libc::RT_TABLE_MAIN,
        rtm_protocol: libc::RTPROT_BOOT,
        rtm_scope: scope,
        rtm_type: libc::RTN_UNICAST,
        rtm_flags: 0,
    }
}

pub fn if_index(dev: &str) -> u32 {
    let dev = CString::new(dev).expect("interface name must not contain NUL");
    let index = unsafe { libc::if_nametoindex(dev.as_ptr()) };
    assert_ne!(index, 0, "failed to resolve ifindex for interface");
    index
}

fn send_rtnl_request(
    description: &str,
    message_type: u16,
    flags: u16,
    build_payload: impl FnOnce(&mut Vec<u8>),
) {
    try_send_rtnl_request(message_type, flags, build_payload)
        .unwrap_or_else(|err| panic!("{description} failed: {err}"));
}

fn try_send_rtnl_request(
    message_type: u16,
    flags: u16,
    build_payload: impl FnOnce(&mut Vec<u8>),
) -> io::Result<()> {
    let sock = socket(
        AddressFamily::Netlink,
        SockType::Raw,
        SockFlag::empty(),
        SockProtocol::NetlinkRoute,
    )
    .map_err(io::Error::from)?;

    let mut request = Vec::new();
    let header = libc::nlmsghdr {
        nlmsg_len: 0,
        nlmsg_type: message_type,
        nlmsg_flags: flags,
        nlmsg_seq: 1,
        nlmsg_pid: 0,
    };
    request.extend_from_slice(bytes_of(&header));
    build_payload(&mut request);
    write_nlmsg_len(&mut request)?;

    let kernel = NetlinkAddr::new(0, 0);
    let sent =
        sendto(sock.as_raw_fd(), &request, &kernel, MsgFlags::empty()).map_err(io::Error::from)?;
    if sent != request.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short netlink datagram send",
        ));
    }

    recv_ack(sock.as_raw_fd(), header.nlmsg_seq)
}

fn recv_ack(fd: i32, seq: u32) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let len = recv(fd, &mut buf, MsgFlags::empty()).map_err(io::Error::from)?;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "netlink socket closed before ACK",
            ));
        }

        let mut offset = 0;
        while offset < len {
            let header = read_nlmsghdr(&buf[offset..len])?;
            let message_len = header.nlmsg_len as usize;
            let message_end = offset
                .checked_add(message_len)
                .ok_or_else(|| io::Error::other("netlink message length overflows offset"))?;
            if header.nlmsg_seq == seq && header.nlmsg_type == libc::NLMSG_ERROR as u16 {
                return parse_netlink_error(&buf[offset..message_end]);
            }

            offset = offset
                .checked_add(align_to(message_len, NLMSG_ALIGNTO))
                .ok_or_else(|| io::Error::other("aligned netlink message length overflows"))?;
        }
    }
}

fn parse_netlink_error(message: &[u8]) -> io::Result<()> {
    let error_offset = mem::size_of::<libc::nlmsghdr>();
    let error_end = error_offset
        .checked_add(mem::size_of::<libc::nlmsgerr>())
        .ok_or_else(|| io::Error::other("netlink error offset overflows"))?;
    if message.len() < error_end {
        return Err(io::Error::other("truncated netlink error"));
    }

    let error =
        unsafe { ptr::read_unaligned(message[error_offset..].as_ptr() as *const libc::nlmsgerr) };
    if error.error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(
            error
                .error
                .checked_neg()
                .ok_or_else(|| io::Error::other("invalid netlink error code"))?,
        ))
    }
}

fn read_nlmsghdr(buf: &[u8]) -> io::Result<libc::nlmsghdr> {
    if buf.len() < mem::size_of::<libc::nlmsghdr>() {
        return Err(io::Error::other("buffer smaller than nlmsghdr"));
    }

    let header = unsafe { ptr::read_unaligned(buf.as_ptr() as *const libc::nlmsghdr) };
    let message_len = header.nlmsg_len as usize;
    if message_len < mem::size_of::<libc::nlmsghdr>() || message_len > buf.len() {
        return Err(io::Error::other("invalid netlink message length"));
    }
    Ok(header)
}

fn write_nlmsg_len(request: &mut [u8]) -> io::Result<()> {
    if request.len() > u32::MAX as usize {
        return Err(io::Error::other("netlink request too large"));
    }

    let len = (request.len() as u32).to_ne_bytes();
    request[..len.len()].copy_from_slice(&len);
    Ok(())
}

fn push_attr(buf: &mut Vec<u8>, attr_type: u16, payload: &[u8]) {
    let attr_len = NLA_HDR_LEN
        .checked_add(payload.len())
        .expect("netlink attribute length overflows");
    assert!(
        attr_len <= u16::MAX as usize,
        "netlink attribute is too large"
    );
    let attr = libc::nlattr {
        nla_len: attr_len as u16,
        nla_type: attr_type,
    };
    buf.extend_from_slice(bytes_of(&attr));
    buf.extend_from_slice(payload);
    pad_to_alignment(buf, NLMSG_ALIGNTO);
}

fn push_nested_attr(buf: &mut Vec<u8>, attr_type: u16, build_payload: impl FnOnce(&mut Vec<u8>)) {
    let start = buf.len();
    let attr = libc::nlattr {
        nla_len: 0,
        nla_type: attr_type,
    };
    buf.extend_from_slice(bytes_of(&attr));
    build_payload(buf);

    let attr_len = buf
        .len()
        .checked_sub(start)
        .expect("nested netlink attribute start exceeds buffer length");
    assert!(
        attr_len <= u16::MAX as usize,
        "nested netlink attribute is too large"
    );
    let len = (attr_len as u16).to_ne_bytes();
    let len_end = start
        .checked_add(len.len())
        .expect("nested netlink attribute header length overflows");
    buf[start..len_end].copy_from_slice(&len);
    pad_to_alignment(buf, NLMSG_ALIGNTO);
}

fn pad_to_alignment(buf: &mut Vec<u8>, align: usize) {
    buf.resize(align_to(buf.len(), align), 0);
}

fn ifname_attr(name: &str) -> Vec<u8> {
    assert!(
        name.len() < libc::IFNAMSIZ,
        "interface name `{name}` is too long"
    );
    CString::new(name)
        .expect("interface name must not contain NUL")
        .as_bytes_with_nul()
        .to_vec()
}

fn write_fixed_ifname(buf: &mut [libc::c_char; libc::IFNAMSIZ], name: &str) {
    assert!(
        name.len() < libc::IFNAMSIZ,
        "interface name `{name}` is too long"
    );
    for (dst, src) in buf.iter_mut().zip(name.bytes()) {
        *dst = src as libc::c_char;
    }
}

fn parse_ipv4_prefix(prefix: &str) -> (Ipv4Addr, u8) {
    let (addr, prefix_len) = prefix
        .split_once('/')
        .unwrap_or_else(|| panic!("IPv4 prefix `{prefix}` must contain /"));
    let addr = addr
        .parse()
        .unwrap_or_else(|err| panic!("invalid IPv4 prefix address `{addr}`: {err}"));
    let prefix_len: u8 = prefix_len
        .parse()
        .unwrap_or_else(|err| panic!("invalid IPv4 prefix length `{prefix_len}`: {err}"));
    assert!(prefix_len <= 32, "invalid IPv4 prefix length {prefix_len}");
    (addr, prefix_len)
}

fn bytes_of<T>(value: &T) -> &[u8] {
    let size = mem::size_of::<T>();
    unsafe { slice::from_raw_parts(slice::from_ref(value).as_ptr().cast(), size) }
}

const fn align_to(value: usize, align: usize) -> usize {
    value.wrapping_add(align.wrapping_sub(1)) & !align.wrapping_sub(1)
}
