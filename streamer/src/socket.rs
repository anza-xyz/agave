//! Re-export SocketAddrSpace from net-utils for backward compatibility

#[deprecated(
    since = "4.0.0",
    note = "Use `solana_net_utils::socket_addr_space::SocketAddrSpace` instead"
)]
pub use solana_net_utils::socket_addr_space::SocketAddrSpace;