use {
    crate::{
        crds_filter::{GossipFilterDirection, should_retain_crds_value},
        crds_value::CrdsValue,
        protocol::Protocol,
        sigverify_cache::SigVerifyCache,
    },
    solana_pubkey::Pubkey,
    solana_sanitize::Sanitize,
    std::{collections::HashMap, net::SocketAddr},
};

/// The CRDS values a message carries, if its type carries any.
fn crds_values(protocol: &mut Protocol) -> Option<&mut Vec<CrdsValue>> {
    match protocol {
        Protocol::PullResponse(_, values) | Protocol::PushMessage(_, values) => Some(values),
        _ => None,
    }
}

/// A decoded message validated independently of local CRDS state.
/// Only this type crosses the ingress-to-engine boundary.
pub(crate) struct ValidatedGossipMessage {
    from_addr: SocketAddr,
    protocol: Protocol,
}

impl ValidatedGossipMessage {
    pub(crate) fn new(
        from_addr: SocketAddr,
        mut protocol: Protocol,
        stakes: &HashMap<Pubkey, u64>,
        is_full_alpenglow_epoch: bool,
        cache: &SigVerifyCache,
    ) -> Option<Self> {
        protocol.sanitize().ok()?;
        if let Some(values) = crds_values(&mut protocol) {
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
        protocol.verify(cache).then_some(Self {
            from_addr,
            protocol,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_unchecked(from_addr: SocketAddr, protocol: Protocol) -> Self {
        Self {
            from_addr,
            protocol,
        }
    }

    /// Retains matching CRDS values and returns the number dropped.
    pub(crate) fn retain_crds_values(
        &mut self,
        predicate: impl FnMut(&CrdsValue) -> bool,
    ) -> usize {
        let Some(values) = crds_values(&mut self.protocol) else {
            return 0;
        };
        let original_len = values.len();
        values.retain(predicate);
        original_len - values.len()
    }

    pub(crate) fn protocol(&self) -> &Protocol {
        &self.protocol
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
    fn test_validation_produces_message() {
        let keypair = Keypair::new();
        let value = CrdsValue::new(
            CrdsData::from(ContactInfo::new_localhost(&keypair.pubkey(), 1)),
            &keypair,
        );
        let protocol = Protocol::PushMessage(keypair.pubkey(), vec![value]);
        let from_addr = "127.0.0.1:1234".parse().unwrap();
        let message = ValidatedGossipMessage::new(
            from_addr,
            protocol,
            &HashMap::new(),
            false,
            &SigVerifyCache::new(),
        )
        .unwrap();
        assert_eq!(message.into_parts().0, from_addr);
    }
}
