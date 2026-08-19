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
        crate::{
            contact_info::ContactInfo,
            crds_data::{CrdsData, LowestSlot},
            crds_value::CrdsValue,
            epoch_slots::EpochSlots,
        },
        solana_keypair::Keypair,
        solana_signer::Signer,
    };

    #[test]
    fn test_validation() {
        let keypair = Keypair::new();
        let contact_info = CrdsValue::new(
            CrdsData::from(ContactInfo::new_localhost(&keypair.pubkey(), 1)),
            &keypair,
        );
        let epoch_slots = CrdsValue::new(
            CrdsData::EpochSlots(0, EpochSlots::new(keypair.pubkey(), 1)),
            &keypair,
        );
        let from_addr = "127.0.0.1:1234".parse().unwrap();
        let cache = SigVerifyCache::new();

        let protocol = Protocol::PushMessage(
            keypair.pubkey(),
            vec![epoch_slots.clone(), contact_info.clone()],
        );
        let message =
            ValidatedGossipMessage::new(from_addr, protocol, &HashMap::new(), true, &cache)
                .unwrap();
        let (actual_addr, protocol) = message.into_parts();
        assert_eq!(actual_addr, from_addr);
        let Protocol::PushMessage(sender, values) = protocol else {
            panic!("expected push message");
        };
        assert_eq!(sender, keypair.pubkey());
        assert_eq!(values, vec![contact_info]);

        let protocol = Protocol::PushMessage(keypair.pubkey(), vec![epoch_slots]);
        assert!(
            ValidatedGossipMessage::new(from_addr, protocol, &HashMap::new(), true, &cache)
                .is_none()
        );

        let malformed = CrdsValue::new(
            CrdsData::LowestSlot(1, LowestSlot::new(keypair.pubkey(), 0, 1)),
            &keypair,
        );
        let protocol = Protocol::PushMessage(keypair.pubkey(), vec![malformed]);
        assert!(
            ValidatedGossipMessage::new(from_addr, protocol, &HashMap::new(), false, &cache)
                .is_none()
        );

        let wrong_keypair = Keypair::new();
        let invalid_signature = CrdsValue::new(
            CrdsData::from(ContactInfo::new_localhost(&keypair.pubkey(), 1)),
            &wrong_keypair,
        );
        let protocol = Protocol::PushMessage(keypair.pubkey(), vec![invalid_signature]);
        assert!(
            ValidatedGossipMessage::new(from_addr, protocol, &HashMap::new(), false, &cache)
                .is_none()
        );
    }
}
