use {
    crate::{
        crds_filter::{GossipFilterDirection, should_retain_crds_value},
        protocol::Protocol,
        sigverify_cache::SigVerifyCache,
    },
    solana_pubkey::Pubkey,
    solana_sanitize::Sanitize,
    std::{collections::HashMap, net::SocketAddr},
};

/// A decoded message that has passed structural sanitization.
pub(crate) struct SanitizedGossipMessage {
    from_addr: SocketAddr,
    protocol: Protocol,
}

/// A sanitized message whose CRDS values are allowed in the current epoch.
pub(crate) struct FilteredGossipMessage {
    from_addr: SocketAddr,
    protocol: Protocol,
}

/// The only message type accepted by the gossip processing thread.
pub(crate) struct VerifiedGossipMessage {
    from_addr: SocketAddr,
    protocol: Protocol,
}

impl SanitizedGossipMessage {
    pub(crate) fn new(from_addr: SocketAddr, protocol: Protocol) -> Option<Self> {
        protocol.sanitize().ok()?;
        Some(Self {
            from_addr,
            protocol,
        })
    }

    pub(crate) fn filter_crds_values(
        mut self,
        stakes: &HashMap<Pubkey, u64>,
        is_full_alpenglow_epoch: bool,
    ) -> Option<FilteredGossipMessage> {
        if let Protocol::PullResponse(_, values) | Protocol::PushMessage(_, values) =
            &mut self.protocol
        {
            values.retain(|value| {
                should_retain_crds_value(
                    value,
                    stakes,
                    GossipFilterDirection::Ingress,
                    is_full_alpenglow_epoch,
                )
            });
            if values.is_empty() {
                return None;
            }
        }
        Some(FilteredGossipMessage {
            from_addr: self.from_addr,
            protocol: self.protocol,
        })
    }
}

impl FilteredGossipMessage {
    pub(crate) fn verify(self, cache: &SigVerifyCache) -> Option<VerifiedGossipMessage> {
        self.protocol
            .verify(cache)
            .then_some(VerifiedGossipMessage {
                from_addr: self.from_addr,
                protocol: self.protocol,
            })
    }
}

impl VerifiedGossipMessage {
    pub(crate) fn protocol_mut(&mut self) -> &mut Protocol {
        &mut self.protocol
    }

    pub(crate) fn into_parts(self) -> (SocketAddr, Protocol) {
        (self.from_addr, self.protocol)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{contact_info::ContactInfo, crds_data::CrdsData, crds_value::CrdsValue},
        solana_keypair::Keypair,
        solana_signer::Signer,
    };

    #[test]
    fn test_validation_stages_produce_verified_message() {
        let keypair = Keypair::new();
        let value = CrdsValue::new(
            CrdsData::from(ContactInfo::new_localhost(&keypair.pubkey(), 1)),
            &keypair,
        );
        let protocol = Protocol::PushMessage(keypair.pubkey(), vec![value]);
        let from_addr = "127.0.0.1:1234".parse().unwrap();

        let message = SanitizedGossipMessage::new(from_addr, protocol)
            .unwrap()
            .filter_crds_values(&HashMap::new(), false)
            .unwrap()
            .verify(&SigVerifyCache::new())
            .unwrap();
        assert_eq!(message.into_parts().0, from_addr);
    }
}
