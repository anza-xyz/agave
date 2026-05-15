#![cfg(target_os = "linux")]

use {
    agave_xdp::{
        device::{NetworkDevice, QueueId, RingSizes},
        load_xdp_program,
        socket::Socket,
        transmitter::{TransmitterBuilder, XdpConfig},
        umem::{OwnedUmem, PageAlignedMemory},
    },
    std::{
        env,
        sync::{
            Arc, Mutex, MutexGuard,
            atomic::{AtomicBool, Ordering},
        },
    },
};

const INTERFACE_ENV: &str = "AGAVE_XDP_ZC_INTERFACE";

static HARDWARE_ZERO_COPY_TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug)]
struct ZeroCopyConfig {
    interface: String,
}

impl ZeroCopyConfig {
    fn from_env() -> Option<Self> {
        let Some(interface) = env::var_os(INTERFACE_ENV) else {
            eprintln!("skipping hardware zero-copy test: {INTERFACE_ENV} is not set");
            return None;
        };
        let interface = interface
            .into_string()
            .unwrap_or_else(|_| panic!("{INTERFACE_ENV} must be valid UTF-8"));
        assert!(
            !interface.is_empty(),
            "{INTERFACE_ENV} must not be empty when set"
        );
        Some(Self { interface })
    }
}

fn transmitter_cpu() -> usize {
    let cores = core_affinity::get_core_ids().expect("linux provides affine cores");
    assert!(
        cores.len() >= 2,
        "hardware zero-copy transmitter test requires at least two CPU cores"
    );
    cores[0].id
}

fn hardware_zero_copy_test_lock() -> MutexGuard<'static, ()> {
    HARDWARE_ZERO_COPY_TEST_LOCK
        .lock()
        .expect("hardware zero-copy test lock must not be poisoned")
}

#[test]
fn configured_interface_binds_af_xdp_zero_copy() {
    let Some(config) = ZeroCopyConfig::from_env() else {
        return;
    };
    let _lock = hardware_zero_copy_test_lock();
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "hardware zero-copy test requires root or equivalent capabilities",
    );

    let dev = NetworkDevice::new(&config.interface).unwrap_or_else(|err| {
        panic!(
            "open hardware zero-copy interface {}: {err}",
            config.interface
        )
    });
    let _xdp_program = load_xdp_program(&dev).unwrap_or_else(|err| {
        panic!(
            "attach XDP pass program to hardware zero-copy interface {}: {err}",
            config.interface
        )
    });
    let queue_id = QueueId(0);
    let queue = dev.open_queue(queue_id).unwrap_or_else(|err| {
        panic!(
            "open queue {:?} on hardware zero-copy interface {}: {err}",
            queue_id, config.interface
        )
    });
    let ring_sizes = queue.ring_sizes().unwrap_or_else(|| {
        panic!(
            "hardware zero-copy interface {} must report ring sizes",
            config.interface
        )
    });
    let frame_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let memory = PageAlignedMemory::alloc(frame_size, umem_frame_count(ring_sizes))
        .expect("allocate xdp umem for hardware zero-copy interface");
    let umem = OwnedUmem::new(memory, frame_size as u32)
        .expect("create xdp umem for hardware zero-copy interface");
    let ring_size = ring_sizes.tx.max(1).next_power_of_two();

    let (socket, _tx) = Socket::tx(queue, umem, true, ring_size, ring_size).unwrap_or_else(|err| {
        panic!(
            "bind AF_XDP zero-copy socket on {} queue {:?}: {err}",
            config.interface, queue_id,
        )
    });
    assert!(
        socket
            .zero_copy_enabled()
            .expect("read AF_XDP socket options")
    );
}

fn umem_frame_count(ring_sizes: RingSizes) -> usize {
    ring_sizes
        .rx
        .saturating_add(ring_sizes.tx)
        .saturating_mul(2)
        .max(1)
        .next_power_of_two()
}

#[test]
fn configured_interface_builds_zero_copy_transmitter() {
    let Some(config) = ZeroCopyConfig::from_env() else {
        return;
    };
    let _lock = hardware_zero_copy_test_lock();
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "hardware zero-copy test requires root or equivalent capabilities",
    );

    let exit = Arc::new(AtomicBool::new(false));
    let mut xdp_config = XdpConfig::new(
        Some(config.interface.clone()),
        vec![transmitter_cpu()],
        true,
    );
    xdp_config.tx_channel_cap = 16;
    let (transmitter, sender) = TransmitterBuilder::new(xdp_config, Arc::clone(&exit))
        .unwrap_or_else(|err| {
            panic!(
                "build zero-copy transmitter on hardware interface {}: {err}",
                config.interface,
            )
        })
        .build();

    exit.store(true, Ordering::Relaxed);
    drop(sender);
    transmitter
        .join()
        .expect("join zero-copy transmitter threads");
}
