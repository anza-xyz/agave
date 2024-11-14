use {
    crate::contact_info::ContactInfo,
    solana_client::connection_cache::Protocol,
    solana_sdk::pubkey::Pubkey,
    std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    thiserror::Error,
};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum FfiProtocol {
    UDP,
    QUIC,
}

impl From<FfiProtocol> for Protocol {
    fn from(ffi_protocol: FfiProtocol) -> Self {
        match ffi_protocol {
            FfiProtocol::UDP => Protocol::UDP,
            FfiProtocol::QUIC => Protocol::QUIC,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FfiVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    pub commit: u32,
    pub feature_set: u32,
    pub client: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FfiIpAddr {
    pub is_v4: u8,      // 1 if IPv4, 0 if IPv6
    pub addr: [u8; 16], // IP address bytes
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FfiSocketAddr {
    pub ip: FfiIpAddr,
    pub port: u16,
}

// Convert Rust IpAddr to and FfiIpAddr
fn ffi_ip_addr_from_ip_addr(ip_addr: &IpAddr) -> FfiIpAddr {
    match ip_addr {
        IpAddr::V4(ipv4) => {
            let mut addr = [0u8; 16];
            addr[..4].copy_from_slice(&ipv4.octets());
            FfiIpAddr { is_v4: 1, addr }
        }
        IpAddr::V6(ipv6) => FfiIpAddr {
            is_v4: 0,
            addr: ipv6.octets(),
        },
    }
}

fn ffi_socket_addr_from_socket_addr(socket_addr: &SocketAddr) -> FfiSocketAddr {
    let ip_addr = socket_addr.ip();
    let ffi_ip_addr = ffi_ip_addr_from_ip_addr(&ip_addr);
    FfiSocketAddr {
        ip: ffi_ip_addr,
        port: socket_addr.port(),
    }
}

// Convert FfiSocketAddr to SocketAddr
pub fn ffi_socket_addr_to_socket_addr(ffi_socket: &FfiSocketAddr) -> SocketAddr {
    let ip_addr = if ffi_socket.ip.is_v4 == 1 {
        IpAddr::V4(Ipv4Addr::new(
            ffi_socket.ip.addr[0],
            ffi_socket.ip.addr[1],
            ffi_socket.ip.addr[2],
            ffi_socket.ip.addr[3],
        ))
    } else {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&ffi_socket.ip.addr);
        IpAddr::V6(Ipv6Addr::from(octets))
    };

    SocketAddr::new(ip_addr, ffi_socket.port)
}

// Define the interface struct
#[repr(C)]
pub struct FfiContactInfoInterface {
    pub contact_info_ptr: ContactInfoPtr,
    pub get_pubkey_fn: ContactInfoGetKey,
    pub get_wallclock_fn: ContactInfoGetWallclockFn,
    pub get_shred_version_fn: ContactInfoGetShredVersionFn,
    pub get_version_fn: ContactInfoGetVersionFn,

    // Socket address getter functions
    pub get_gossip_fn: ContactInfoGetGossipFn,
    pub get_rpc_fn: ContactInfoGetRpcFn,
    pub get_rpc_pubsub_fn: ContactInfoGetRpcPubsubpFn,
    pub get_serve_repair_fn: ContactInfoGetServeRepairFn,
    pub get_tpu_fn: ContactInfoGetTpuFn,
    pub get_tpu_forwards_fn: ContactInfoGetTpuForwardsFn,
    pub get_tpu_vote_fn: ContactInfoGetTpuVoteFn,
    pub get_tvu_fn: ContactInfoGetTvuFn,
}

/// The key is a 32-byte array that represents a public key.
pub type Key = *const u8;

/// An opaque pointer to a ContactInfo. This will be provided to any plugin and
/// should always be non-null. The plugin should not pass any pointers to any
/// functions on the [`ContactInfoInterface`] that are not provided.
pub type ContactInfoPtr = *const core::ffi::c_void;

// Define function pointer types for the interface
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetKey = unsafe extern "C" fn(contact_info_ptr: ContactInfoPtr) -> Key;

/// Returns wallclock of ContactInfo
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetWallclockFn = unsafe extern "C" fn(contact_info_ptr: ContactInfoPtr) -> u64;

/// Returns shred version of ContactInfo
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetShredVersionFn =
    unsafe extern "C" fn(contact_info_ptr: ContactInfoPtr) -> u16;

/// Returns version of ContactInfo
/// TODO: figure out if we can fix this to not take in a *mut FfiVersion
/// although this may be the best bet. Returning a *const FfiVersion is
/// hard because we allocate it on the heap and we can't guarantee that
/// the caller will free it.
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetVersionFn =
    unsafe extern "C" fn(contact_info_ptr: ContactInfoPtr, ffi_version: *mut FfiVersion) -> bool;

/// Returns gossip address of ContactInfo
/// TODO: same as above
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetGossipFn =
    unsafe extern "C" fn(contact_info_ptr: ContactInfoPtr, socket: *mut FfiSocketAddr) -> bool;

/// Returns rpc address of ContactInfo
/// TODO: same as above
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetRpcFn =
    unsafe extern "C" fn(contact_info_ptr: ContactInfoPtr, socket: *mut FfiSocketAddr) -> bool;

/// Returns rpc_pubsub address of ContactInfo
/// TODO: same as above
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetRpcPubsubpFn =
    unsafe extern "C" fn(contact_info_ptr: ContactInfoPtr, socket: *mut FfiSocketAddr) -> bool;

/// Returns serve_repair address of ContactInfo
/// TODO: same as above
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetServeRepairFn = unsafe extern "C" fn(
    contact_info_ptr: ContactInfoPtr,
    protocol: FfiProtocol,
    socket: *mut FfiSocketAddr,
) -> bool;

/// Returns tpu address of ContactInfo
/// TODO: same as above
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetTpuFn = unsafe extern "C" fn(
    contact_info_ptr: ContactInfoPtr,
    protocol: FfiProtocol,
    socket: *mut FfiSocketAddr,
) -> bool;

/// Returns tpu_forwards address of ContactInfo
/// TODO: same as above
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetTpuForwardsFn = unsafe extern "C" fn(
    contact_info_ptr: ContactInfoPtr,
    protocol: FfiProtocol,
    socket: *mut FfiSocketAddr,
) -> bool;

/// Returns tpu_vote address of ContactInfo
/// TODO: same as above
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetTpuVoteFn = unsafe extern "C" fn(
    contact_info_ptr: ContactInfoPtr,
    protocol: FfiProtocol,
    socket: *mut FfiSocketAddr,
) -> bool;

/// Returns tvu address of ContactInfo
/// TODO: same as above
/// # Safety
/// - The ContactInfo pointer must be valid.
pub type ContactInfoGetTvuFn = unsafe extern "C" fn(
    contact_info_ptr: ContactInfoPtr,
    protocol: FfiProtocol,
    socket: *mut FfiSocketAddr,
) -> bool;

/// Given a reference to `ContactInfo`, create a `ContactInfoInterface` that
/// can be used to interact with the ContactInfo struct in C-compatible code.
/// # Safety
/// This interface is only valid for the lifetime of the ContactInfo
/// reference, which cannot be guaranteed by this function interface.
pub unsafe fn create_contact_info_interface(contact_info: &ContactInfo) -> FfiContactInfoInterface {
    extern "C" fn get_pubkey(contact_info_ptr: ContactInfoPtr) -> Key {
        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        contact_info.pubkey().as_ref().as_ptr()
    }

    extern "C" fn get_wallclock(contact_info_ptr: ContactInfoPtr) -> u64 {
        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        contact_info.wallclock()
    }

    extern "C" fn get_shred_version(contact_info_ptr: ContactInfoPtr) -> u16 {
        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        contact_info.shred_version()
    }

    extern "C" fn get_version(
        contact_info_ptr: ContactInfoPtr,
        ffi_version: *mut FfiVersion,
    ) -> bool {
        if contact_info_ptr.is_null() || ffi_version.is_null() {
            return false;
        }

        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        let version = contact_info.version();

        unsafe {
            (*ffi_version).major = version.major;
            (*ffi_version).minor = version.minor;
            (*ffi_version).patch = version.patch;
            (*ffi_version).commit = version.commit;
            (*ffi_version).feature_set = version.feature_set;
            (*ffi_version).client = u16::try_from(version.client()).unwrap();
        }
        true
    }

    // Socket address getter functions
    // replicates gossip(), rpc(), etc in ContactInfo
    extern "C" fn get_gossip(contact_info_ptr: ContactInfoPtr, socket: *mut FfiSocketAddr) -> bool {
        if contact_info_ptr.is_null() || socket.is_null() {
            return false;
        }

        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        match contact_info.gossip() {
            Ok(socket_addr) => {
                let ffi_socket_addr = ffi_socket_addr_from_socket_addr(&socket_addr);
                unsafe { *socket = ffi_socket_addr };
                true
            }
            Err(_) => false,
        }
    }

    extern "C" fn get_rpc(contact_info_ptr: ContactInfoPtr, socket: *mut FfiSocketAddr) -> bool {
        if contact_info_ptr.is_null() || socket.is_null() {
            return false;
        }

        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        match contact_info.rpc() {
            Ok(socket_addr) => {
                let ffi_socket_addr = ffi_socket_addr_from_socket_addr(&socket_addr);
                unsafe { *socket = ffi_socket_addr };
                true
            }
            Err(_) => false,
        }
    }

    extern "C" fn get_rpc_pubsub(
        contact_info_ptr: ContactInfoPtr,
        socket: *mut FfiSocketAddr,
    ) -> bool {
        if contact_info_ptr.is_null() || socket.is_null() {
            return false;
        }

        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        match contact_info.rpc_pubsub() {
            Ok(socket_addr) => {
                let ffi_socket_addr = ffi_socket_addr_from_socket_addr(&socket_addr);
                unsafe { *socket = ffi_socket_addr };
                true
            }
            Err(_) => false,
        }
    }

    extern "C" fn get_serve_repair(
        contact_info_ptr: ContactInfoPtr,
        protocol: FfiProtocol,
        socket: *mut FfiSocketAddr,
    ) -> bool {
        if contact_info_ptr.is_null() || socket.is_null() {
            return false;
        }

        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        let protocol = Protocol::from(protocol); // Convert FfiProtocol to Protocol

        match contact_info.serve_repair(protocol) {
            Ok(socket_addr) => {
                let ffi_socket_addr = ffi_socket_addr_from_socket_addr(&socket_addr);
                unsafe { *socket = ffi_socket_addr };
                true
            }
            Err(_) => false,
        }
    }

    extern "C" fn get_tpu(
        contact_info_ptr: ContactInfoPtr,
        protocol: FfiProtocol,
        socket: *mut FfiSocketAddr,
    ) -> bool {
        if contact_info_ptr.is_null() || socket.is_null() {
            return false;
        }

        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        let protocol = Protocol::from(protocol); // Convert FfiProtocol to Protocol

        match contact_info.tpu(protocol) {
            Ok(socket_addr) => {
                let ffi_socket_addr = ffi_socket_addr_from_socket_addr(&socket_addr);
                unsafe { *socket = ffi_socket_addr };
                true
            }
            Err(_) => false,
        }
    }

    extern "C" fn get_tpu_forwards(
        contact_info_ptr: ContactInfoPtr,
        protocol: FfiProtocol,
        socket: *mut FfiSocketAddr,
    ) -> bool {
        if contact_info_ptr.is_null() || socket.is_null() {
            return false;
        }

        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        let protocol = Protocol::from(protocol); // Convert FfiProtocol to Protocol

        match contact_info.tpu_forwards(protocol) {
            Ok(socket_addr) => {
                let ffi_socket_addr = ffi_socket_addr_from_socket_addr(&socket_addr);
                unsafe { *socket = ffi_socket_addr };
                true
            }
            Err(_) => false,
        }
    }

    extern "C" fn get_tpu_vote(
        contact_info_ptr: ContactInfoPtr,
        protocol: FfiProtocol,
        socket: *mut FfiSocketAddr,
    ) -> bool {
        if contact_info_ptr.is_null() || socket.is_null() {
            return false;
        }

        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        let protocol = Protocol::from(protocol); // Convert FfiProtocol to Protocol

        match contact_info.tpu_vote(protocol) {
            Ok(socket_addr) => {
                let ffi_socket_addr = ffi_socket_addr_from_socket_addr(&socket_addr);
                unsafe { *socket = ffi_socket_addr };
                true
            }
            Err(_) => false,
        }
    }

    extern "C" fn get_tvu(
        contact_info_ptr: ContactInfoPtr,
        protocol: FfiProtocol,
        socket: *mut FfiSocketAddr,
    ) -> bool {
        if contact_info_ptr.is_null() || socket.is_null() {
            return false;
        }

        let contact_info = unsafe { &*(contact_info_ptr as *const ContactInfo) };
        let protocol = Protocol::from(protocol); // Convert FfiProtocol to Protocol

        match contact_info.tvu(protocol) {
            Ok(socket_addr) => {
                let ffi_socket_addr = ffi_socket_addr_from_socket_addr(&socket_addr);
                unsafe { *socket = ffi_socket_addr };
                true
            }
            Err(_) => false,
        }
    }

    FfiContactInfoInterface {
        contact_info_ptr: contact_info as *const ContactInfo as *const core::ffi::c_void,
        get_pubkey_fn: get_pubkey,
        get_wallclock_fn: get_wallclock,
        get_shred_version_fn: get_shred_version,
        get_version_fn: get_version,
        get_gossip_fn: get_gossip,
        get_rpc_fn: get_rpc,
        get_rpc_pubsub_fn: get_rpc_pubsub,
        get_serve_repair_fn: get_serve_repair,
        get_tpu_fn: get_tpu,
        get_tpu_forwards_fn: get_tpu_forwards,
        get_tpu_vote_fn: get_tpu_vote,
        get_tvu_fn: get_tvu,
    }
}

#[derive(Debug, Error)]
pub enum ContactInfoError {
    #[error("Failed to retrieve version")]
    VersionRetrievalFailed,
    #[error("Failed to retrieve pubkey")]
    PubkeyRetrievalFailed,
}

impl FfiContactInfoInterface {
    pub fn pubkey(&self) -> Result<Pubkey, ContactInfoError> {
        let pubkey_ptr = unsafe { (self.get_pubkey_fn)(self.contact_info_ptr) };
        if pubkey_ptr.is_null() {
            return Err(ContactInfoError::PubkeyRetrievalFailed);
        }
        let pubkey_bytes = unsafe { std::slice::from_raw_parts(pubkey_ptr, 32) };
        let pk =
            Pubkey::try_from(pubkey_bytes).map_err(|_| ContactInfoError::PubkeyRetrievalFailed)?;
        Ok(pk)
    }

    pub fn wallclock(&self) -> u64 {
        unsafe { (self.get_wallclock_fn)(self.contact_info_ptr) }
    }

    pub fn shred_version(&self) -> u16 {
        unsafe { (self.get_shred_version_fn)(self.contact_info_ptr) }
    }

    pub fn version(&self) -> Result<FfiVersion, ContactInfoError> {
        let mut ffi_version = FfiVersion::default();
        let success = unsafe {
            (self.get_version_fn)(self.contact_info_ptr, &mut ffi_version as *mut FfiVersion)
        };
        if success {
            Ok(ffi_version)
        } else {
            Err(ContactInfoError::VersionRetrievalFailed)
        }
    }

    pub fn gossip(&self) -> Option<FfiSocketAddr> {
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (self.get_gossip_fn)(self.contact_info_ptr, &mut ffi_socket as *mut FfiSocketAddr)
        };

        if success {
            Some(ffi_socket)
        } else {
            None
        }
    }

    pub fn rpc(&self) -> Option<FfiSocketAddr> {
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (self.get_rpc_fn)(self.contact_info_ptr, &mut ffi_socket as *mut FfiSocketAddr)
        };

        if success {
            Some(ffi_socket)
        } else {
            None
        }
    }

    pub fn rpc_pubsub(&self) -> Option<FfiSocketAddr> {
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (self.get_rpc_pubsub_fn)(self.contact_info_ptr, &mut ffi_socket as *mut FfiSocketAddr)
        };

        if success {
            Some(ffi_socket)
        } else {
            None
        }
    }

    pub fn serve_repair(&self, protocol: FfiProtocol) -> Option<FfiSocketAddr> {
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (self.get_serve_repair_fn)(
                self.contact_info_ptr,
                protocol,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        if success {
            Some(ffi_socket)
        } else {
            None
        }
    }

    pub fn tpu(&self, protocol: FfiProtocol) -> Option<FfiSocketAddr> {
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (self.get_tpu_fn)(
                self.contact_info_ptr,
                protocol,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        if success {
            Some(ffi_socket)
        } else {
            None
        }
    }

    pub fn tpu_forwards(&self, protocol: FfiProtocol) -> Option<FfiSocketAddr> {
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (self.get_tpu_forwards_fn)(
                self.contact_info_ptr,
                protocol,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        if success {
            Some(ffi_socket)
        } else {
            None
        }
    }

    pub fn tpu_vote(&self, protocol: FfiProtocol) -> Option<FfiSocketAddr> {
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (self.get_tpu_vote_fn)(
                self.contact_info_ptr,
                protocol,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        if success {
            Some(ffi_socket)
        } else {
            None
        }
    }

    pub fn tvu(&self, protocol: FfiProtocol) -> Option<FfiSocketAddr> {
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (self.get_tvu_fn)(
                self.contact_info_ptr,
                protocol,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        if success {
            Some(ffi_socket)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_sdk::pubkey::Pubkey};

    #[test]
    fn test_get_pubkey() {
        let mut rng = rand::thread_rng();
        let node_pubkey = Pubkey::new_unique();
        let contact_info = ContactInfo::new_rand(&mut rng, Some(node_pubkey));

        let interface = unsafe { create_contact_info_interface(&contact_info) };
        let pk = interface.pubkey().unwrap();

        assert_eq!(pk, *contact_info.pubkey());
    }

    #[test]
    fn test_get_wallclock() {
        let contact_info =
            ContactInfo::new(solana_sdk::pubkey::Pubkey::new_unique(), 123456789, 23);
        let interface = unsafe { create_contact_info_interface(&contact_info) };

        assert_eq!(interface.wallclock(), contact_info.wallclock());
    }

    #[test]
    fn test_get_shred_version() {
        let contact_info =
            ContactInfo::new(solana_sdk::pubkey::Pubkey::new_unique(), 123456789, 23);
        let interface = unsafe { create_contact_info_interface(&contact_info) };

        assert_eq!(interface.shred_version(), contact_info.shred_version());
    }

    #[test]
    fn test_get_version() {
        let contact_info =
            ContactInfo::new(solana_sdk::pubkey::Pubkey::new_unique(), 123456789, 23);
        let interface = unsafe { create_contact_info_interface(&contact_info) };

        let mut ffi_version = FfiVersion::default();

        let success = unsafe {
            (interface.get_version_fn)(
                interface.contact_info_ptr,
                &mut ffi_version as *mut FfiVersion,
            )
        };

        assert!(success);
        assert_eq!(ffi_version.major, contact_info.version().major);
        assert_eq!(ffi_version.minor, contact_info.version().minor);
        assert_eq!(ffi_version.patch, contact_info.version().patch);
        assert_eq!(ffi_version.commit, contact_info.version().commit);
        assert_eq!(ffi_version.feature_set, contact_info.version().feature_set);
        assert_eq!(
            ffi_version.client,
            u16::try_from(contact_info.version().client()).unwrap()
        );
    }

    #[test]
    fn test_get_gossip() {
        let contact_info = ContactInfo::new_localhost(&Pubkey::new_unique(), 123456789);
        let interface = unsafe { create_contact_info_interface(&contact_info) };
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_gossip_fn)(
                interface.contact_info_ptr,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.gossip().unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);
    }

    #[test]
    fn test_get_rpc() {
        let contact_info = ContactInfo::new_localhost(&Pubkey::new_unique(), 123456789);
        let interface = unsafe { create_contact_info_interface(&contact_info) };
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_rpc_fn)(
                interface.contact_info_ptr,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.rpc().unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);
    }

    #[test]
    fn test_get_rpc_pubsub() {
        let contact_info = ContactInfo::new_localhost(&Pubkey::new_unique(), 123456789);
        let interface = unsafe { create_contact_info_interface(&contact_info) };
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_rpc_pubsub_fn)(
                interface.contact_info_ptr,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.rpc_pubsub().unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);
    }

    #[test]
    fn test_get_serve_repair() {
        let contact_info = ContactInfo::new_localhost(&Pubkey::new_unique(), 123456789);
        let interface = unsafe { create_contact_info_interface(&contact_info) };
        // test udp
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_serve_repair_fn)(
                interface.contact_info_ptr,
                FfiProtocol::UDP,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.serve_repair(Protocol::UDP).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);

        // test quic
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_serve_repair_fn)(
                interface.contact_info_ptr,
                FfiProtocol::QUIC,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.serve_repair(Protocol::QUIC).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);
    }

    #[test]
    fn test_get_tpu() {
        let contact_info = ContactInfo::new_localhost(&Pubkey::new_unique(), 123456789);
        let interface = unsafe { create_contact_info_interface(&contact_info) };
        // test udp
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_tpu_fn)(
                interface.contact_info_ptr,
                FfiProtocol::UDP,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.tpu(Protocol::UDP).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);

        // test quic
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_tpu_fn)(
                interface.contact_info_ptr,
                FfiProtocol::QUIC,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.tpu(Protocol::QUIC).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);
    }

    #[test]
    fn test_get_tpu_forwards() {
        let contact_info = ContactInfo::new_localhost(&Pubkey::new_unique(), 123456789);
        let interface = unsafe { create_contact_info_interface(&contact_info) };
        // test udp
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_tpu_forwards_fn)(
                interface.contact_info_ptr,
                FfiProtocol::UDP,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.tpu_forwards(Protocol::UDP).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);

        // test quic
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_tpu_forwards_fn)(
                interface.contact_info_ptr,
                FfiProtocol::QUIC,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.tpu_forwards(Protocol::QUIC).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);
    }

    #[test]
    fn test_get_tpu_vote() {
        let mut contact_info = ContactInfo::new_localhost(&Pubkey::new_unique(), 123456789);
        let interface = unsafe { create_contact_info_interface(&contact_info) };
        // test udp
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_tpu_vote_fn)(
                interface.contact_info_ptr,
                FfiProtocol::UDP,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.tpu_vote(Protocol::UDP).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);

        // test quic
        // TODO: remove once ContactInfo::new_localhost is updated to include set_tpu_vote_quic()
        contact_info
            .set_tpu_vote_quic((Ipv4Addr::LOCALHOST, 8009))
            .unwrap();
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_tpu_vote_fn)(
                interface.contact_info_ptr,
                FfiProtocol::QUIC,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.tpu_vote(Protocol::QUIC).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);
    }

    #[test]
    fn test_get_tvu() {
        let contact_info = ContactInfo::new_localhost(&Pubkey::new_unique(), 123456789);
        let interface = unsafe { create_contact_info_interface(&contact_info) };
        // test udp
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_tvu_fn)(
                interface.contact_info_ptr,
                FfiProtocol::UDP,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.tvu(Protocol::UDP).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);

        // test quic
        let mut ffi_socket = FfiSocketAddr::default();

        let success = unsafe {
            (interface.get_tvu_fn)(
                interface.contact_info_ptr,
                FfiProtocol::QUIC,
                &mut ffi_socket as *mut FfiSocketAddr,
            )
        };

        assert!(success);

        let expected_socket_addr = contact_info.tvu(Protocol::QUIC).unwrap();
        let actual_socket_addr = ffi_socket_addr_to_socket_addr(&ffi_socket);

        assert_eq!(expected_socket_addr, actual_socket_addr);
    }
}
