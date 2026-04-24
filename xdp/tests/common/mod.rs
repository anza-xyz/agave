#![cfg(target_os = "linux")]
#![allow(dead_code)]

use {
    agave_xdp::netlink::MacAddress,
    std::{
        ffi::CString,
        fs::File,
        os::fd::AsRawFd,
        path::{Path, PathBuf},
        process::Command,
        sync::OnceLock,
        thread,
        time::{Duration, Instant},
    },
};

const LEFT_IFACE: &str = "axdp0";
const RIGHT_IFACE: &str = "axdp1";
const BACKUP_LEFT_IFACE: &str = "bxdp0";
const BACKUP_RIGHT_IFACE: &str = "bxdp1";

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TestLinks {
    pub left_name: String,
    pub right_name: String,
    pub left_if_index: u32,
    pub right_if_index: u32,
    pub left_ip: std::net::Ipv4Addr,
    pub right_ip: std::net::Ipv4Addr,
    pub left_mac: MacAddress,
    pub right_mac: MacAddress,
}

pub struct NetNsGuard {
    old_ns: File,
}

impl NetNsGuard {
    pub fn new() -> Self {
        require_root();

        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        let old_ns_path = format!("/proc/self/task/{tid}/ns/net");
        let old_ns = File::open(&old_ns_path)
            .unwrap_or_else(|err| panic!("failed to open {old_ns_path}: {err}"));

        if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
            let err = std::io::Error::last_os_error();
            panic!("failed to unshare network namespace: {err}");
        }

        let netns = Self { old_ns };
        netns.ip(&["link", "set", "lo", "up"]);
        netns
    }

    pub fn ip(&self, args: &[&str]) {
        run_command(ip_command(), args);
    }
}

impl Drop for NetNsGuard {
    fn drop(&mut self) {
        if unsafe { libc::setns(self.old_ns.as_raw_fd(), libc::CLONE_NEWNET) } == 0 {
            return;
        }

        let err = std::io::Error::last_os_error();
        if std::thread::panicking() {
            eprintln!("failed to restore original network namespace: {err}");
        } else {
            panic!("failed to restore original network namespace: {err}");
        }
    }
}

pub fn setup_veth_pair() -> TestLinks {
    setup_veth_pair_named(
        LEFT_IFACE,
        RIGHT_IFACE,
        std::net::Ipv4Addr::new(10, 0, 0, 1),
        std::net::Ipv4Addr::new(10, 0, 0, 2),
        MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01]),
        MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x02]),
    )
}

pub fn setup_backup_veth_pair() -> TestLinks {
    setup_veth_pair_named(
        BACKUP_LEFT_IFACE,
        BACKUP_RIGHT_IFACE,
        std::net::Ipv4Addr::new(10, 0, 1, 1),
        std::net::Ipv4Addr::new(10, 0, 1, 2),
        MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xee, 0x01]),
        MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xee, 0x02]),
    )
}

pub fn setup_veth_pair_named(
    left_name: &str,
    right_name: &str,
    left_ip: std::net::Ipv4Addr,
    right_ip: std::net::Ipv4Addr,
    left_mac: MacAddress,
    right_mac: MacAddress,
) -> TestLinks {
    run_ip(&[
        "link", "add", left_name, "type", "veth", "peer", "name", right_name,
    ]);
    set_link_mac(left_name, &left_mac.to_string());
    set_link_mac(right_name, &right_mac.to_string());
    add_ipv4_addr(&format!("{left_ip}/24"), left_name);
    add_ipv4_addr(&format!("{right_ip}/24"), right_name);
    set_link_up(left_name);
    set_link_up(right_name);

    TestLinks {
        left_name: left_name.to_string(),
        right_name: right_name.to_string(),
        left_if_index: if_index(left_name),
        right_if_index: if_index(right_name),
        left_ip,
        right_ip,
        left_mac,
        right_mac,
    }
}

pub fn add_route(destination: &str, via: std::net::Ipv4Addr, dev: &str) {
    let via = via.to_string();
    run_ip(&["route", "replace", destination, "via", &via, "dev", dev]);
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

fn require_root() {
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "XDP integration tests require root. Use `cargo xtask xdp-test local`.",
    );
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
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program:?} {args:?}: {err}"));
    if output.status.success() {
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    panic!(
        "{program:?} {args:?} failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status, stdout, stderr
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
