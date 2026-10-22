#![cfg(target_os = "linux")]
#![allow(dead_code)]

pub use aya::test_helpers::NetNsGuard;
use {
    agave_xdp::netlink::MacAddress,
    std::{
        ffi::{CString, OsString},
        os::unix::ffi::OsStringExt,
        path::{Path, PathBuf},
        process::{Command, Output},
        sync::OnceLock,
        thread,
        time::{Duration, Instant},
    },
};

pub const LEFT_IFACE: &str = "axdp0";
pub const RIGHT_IFACE: &str = "axdp1";
pub const GRE_IFACE: &str = "gxdp0";

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

pub fn setup_veth_pair_with_tx_queue_count(tx_queue_count: u32) -> TestLinks {
    let left_ip = std::net::Ipv4Addr::new(10, 0, 0, 1);
    let right_ip = std::net::Ipv4Addr::new(10, 0, 0, 2);
    let left_mac = MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01]);
    let right_mac = MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x02]);
    let tx_queue_count = tx_queue_count.to_string();

    run_ip(&[
        "link",
        "add",
        LEFT_IFACE,
        "numtxqueues",
        &tx_queue_count,
        "type",
        "veth",
        "peer",
        "name",
        RIGHT_IFACE,
        "numtxqueues",
        &tx_queue_count,
    ]);
    set_link_mac(LEFT_IFACE, &left_mac.to_string());
    set_link_mac(RIGHT_IFACE, &right_mac.to_string());
    add_ipv4_addr(&format!("{left_ip}/24"), LEFT_IFACE);
    add_ipv4_addr(&format!("{right_ip}/24"), RIGHT_IFACE);
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
    let local = underlay.left_ip.to_string();
    let remote = underlay.right_ip.to_string();
    let overlay_ip = std::net::Ipv4Addr::new(192, 0, 2, 1);

    run_ip(&[
        "tunnel", "add", GRE_IFACE, "mode", "gre", "local", &local, "remote", &remote, "ttl", "64",
    ]);
    add_ipv4_addr(&format!("{overlay_ip}/32"), GRE_IFACE);
    set_link_up(GRE_IFACE);

    TestGreTunnel {
        if_index: if_index(GRE_IFACE),
        local_ip: underlay.left_ip,
        remote_ip: underlay.right_ip,
        overlay_ip,
    }
}

pub fn add_route(destination: &str, via: std::net::Ipv4Addr, dev: &str) {
    let via = via.to_string();
    run_ip(&["route", "replace", destination, "via", &via, "dev", dev]);
}

pub fn add_route_to_dev(destination: &str, dev: &str) {
    run_ip(&["route", "replace", destination, "dev", dev]);
}

pub fn add_route_to_dev_with_src(destination: &str, dev: &str, src: std::net::Ipv4Addr) {
    let src = src.to_string();
    run_ip(&["route", "replace", destination, "dev", dev, "src", &src]);
}

pub fn delete_link(dev: &str) {
    run_ip(&["link", "del", dev]);
}

#[allow(dead_code)]
pub fn delete_route(destination: &str) {
    run_ip(&["route", "del", destination]);
}

pub fn replace_neighbor(ip: std::net::Ipv4Addr, mac: MacAddress, dev: &str) {
    let ip = ip.to_string();
    let mac = mac.to_string();
    run_ip(&[
        "neigh",
        "replace",
        &ip,
        "lladdr",
        &mac,
        "dev",
        dev,
        "nud",
        "permanent",
    ]);
}

pub fn delete_neighbor(ip: std::net::Ipv4Addr, dev: &str) {
    let ip = ip.to_string();
    run_ip(&["neigh", "del", &ip, "dev", dev]);
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

fn set_link_mac(dev: &str, mac: &str) {
    run_ip(&["link", "set", "dev", dev, "address", mac]);
}

fn set_link_up(dev: &str) {
    run_ip(&["link", "set", "dev", dev, "up"]);
}

fn add_ipv4_addr(addr: &str, dev: &str) {
    run_ip(&["addr", "add", addr, "dev", dev]);
}

pub fn if_index(dev: &str) -> u32 {
    let dev = CString::new(dev).expect("interface name must not contain NUL");
    let index = unsafe { libc::if_nametoindex(dev.as_ptr()) };
    assert_ne!(index, 0, "failed to resolve ifindex for interface");
    index
}

fn run_ip(args: &[&str]) {
    run_command(ip_command(), args);
}

fn run_command(program: &Path, args: &[&str]) {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program:?} {args:?}: {err}"));
    if status.success() {
        return;
    }

    panic!(
        "{program:?} {args:?} failed: {}
stdout:
{}
stderr:
{}",
        status,
        OsString::from_vec(stdout).display(),
        OsString::from_vec(stderr).display(),
    );
}

fn ip_command() -> &'static PathBuf {
    static IP_COMMAND: OnceLock<PathBuf> = OnceLock::new();
    IP_COMMAND.get_or_init(|| {
        let mut candidates = std::env::var_os("IP")
            .into_iter()
            .map(PathBuf::from)
            .chain([
                PathBuf::from("/usr/sbin/ip"),
                PathBuf::from("/sbin/ip"),
                PathBuf::from("ip"),
            ]);

        candidates
            .find(|path| path == Path::new("ip") || path.exists())
            .unwrap_or_else(|| PathBuf::from("ip"))
    })
}
