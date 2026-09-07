//! What an operator can set. Everything else is a constant next to the code
//! that reads it.

use {solana_pubkey::Pubkey, std::net::SocketAddr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardConfig {
    pub listen_addr: SocketAddr,
    /// Host names this dashboard answers to. Pinning `Host` is what stops DNS
    /// rebinding, where a name the attacker controls resolves to loopback and
    /// counts as same-origin. Address literals cannot be rebound and are always
    /// accepted; a reverse proxy forwards the name the visitor typed, so that name
    /// must be listed.
    pub allowed_hosts: Vec<String>,
    /// The jito tip payment program, where this validator runs one. The eight tip
    /// accounts are derived from it, since the id differs between clusters. `None`
    /// on plain agave, and then no tips are read.
    pub tip_payment_program_id: Option<Pubkey>,
    /// This validator's commission on tips, in basis points, for the one figure of
    /// what our own blocks earned us.
    pub commission_bps: Option<u16>,
}

impl DashboardConfig {
    /// Answers to `localhost` and to any address literal; a domain has to be
    /// added by the caller.
    pub fn new(listen_addr: SocketAddr) -> Self {
        Self {
            listen_addr,
            allowed_hosts: vec!["localhost".to_string()],
            tip_payment_program_id: None,
            commission_bps: None,
        }
    }
}
