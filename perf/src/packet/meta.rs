#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample};
use {
    bitflags::bitflags,
    serde::{Deserialize, Serialize},
    solana_pubkey::Pubkey,
    std::net::{IpAddr, Ipv4Addr, SocketAddr},
};

bitflags! {
    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
    pub struct PacketFlags: u8 {
        const DISCARD        = 0b0000_0001;
        const FORWARDED      = 0b0000_0010;
        const REPAIR         = 0b0000_0100;
        const SIMPLE_VOTE_TX = 0b0000_1000;
        // Previously used - this can now be re-used for something else.
        const UNUSED_0  = 0b0001_0000;
        // Previously used - this can now be re-used for something else.
        const UNUSED_1 = 0b0010_0000;
        // Previously used - this can now be re-used for something else.
        const UNUSED_2  = 0b0100_0000;
        /// For marking packets from staked nodes
        const FROM_STAKED_NODE = 0b1000_0000;
    }
}

#[cfg(feature = "stable-abi")]
impl solana_frozen_abi::stable_abi::StableAbi for PacketFlags {
    fn random_with_context(
        rng: &mut (impl solana_frozen_abi::rand::RngCore + ?Sized),
        _ctx: (),
    ) -> Self {
        // All bits are defined, but truncate making it future-proof.
        Self::from_bits_truncate(solana_frozen_abi::rand::Rng::random(rng))
    }
}

/// Metadata attached to every packet.
#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[repr(C)]
pub struct Meta {
    pub size: usize,
    pub addr: IpAddr,
    pub port: u16,
    pub flags: PacketFlags,
    remote_pubkey: Pubkey,
}

impl Meta {
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.addr, self.port)
    }

    pub fn set_socket_addr(&mut self, socket_addr: &SocketAddr) {
        self.addr = socket_addr.ip();
        self.port = socket_addr.port();
    }

    pub fn set_from_staked_node(&mut self, from_staked_node: bool) {
        self.flags
            .set(PacketFlags::FROM_STAKED_NODE, from_staked_node);
    }

    #[inline]
    pub fn discard(&self) -> bool {
        self.flags.contains(PacketFlags::DISCARD)
    }

    #[inline]
    pub fn set_discard(&mut self, discard: bool) {
        self.flags.set(PacketFlags::DISCARD, discard);
    }

    #[inline]
    pub fn forwarded(&self) -> bool {
        self.flags.contains(PacketFlags::FORWARDED)
    }

    #[inline]
    pub fn repair(&self) -> bool {
        self.flags.contains(PacketFlags::REPAIR)
    }

    #[inline]
    pub fn is_simple_vote_tx(&self) -> bool {
        self.flags.contains(PacketFlags::SIMPLE_VOTE_TX)
    }

    #[inline]
    pub fn is_from_staked_node(&self) -> bool {
        self.flags.contains(PacketFlags::FROM_STAKED_NODE)
    }

    #[inline]
    pub fn remote_pubkey(&self) -> Option<Pubkey> {
        if self.remote_pubkey == Pubkey::default() {
            None
        } else {
            Some(self.remote_pubkey)
        }
    }

    /// Sets the remote pubkey. Use Pubkey::default() to clear.
    #[inline]
    pub fn set_remote_pubkey(&mut self, pubkey: Pubkey) {
        self.remote_pubkey = pubkey;
    }
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            size: 0,
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
            flags: PacketFlags::empty(),
            remote_pubkey: Pubkey::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_pubkey() {
        let mut meta = Meta::default();
        assert!(meta.remote_pubkey().is_none());
        let pubkey = Pubkey::new_unique();
        meta.set_remote_pubkey(pubkey);
        assert_eq!(meta.remote_pubkey(), Some(pubkey));
        let pubkey = Pubkey::default();
        meta.set_remote_pubkey(pubkey);
        assert!(meta.remote_pubkey().is_none());
    }
}
