#[derive(Debug, Eq, PartialEq)]
pub enum ClientId {
    SolanaLabs,
    JitoLabs,
    Frankendancer,
    Agave,
    AgavePaladin,
    Firedancer,
    AgaveBam,
    Sig,
    // If new variants are added, update From<u16> and TryFrom<ClientId>.
    Unknown(u16),
}

impl From<u16> for ClientId {
    fn from(client: u16) -> Self {
        match client {
            0u16 => Self::SolanaLabs,
            1u16 => Self::JitoLabs,
            2u16 => Self::Frankendancer,
            3u16 => Self::Agave,
            4u16 => Self::AgavePaladin,
            5u16 => Self::Firedancer,
            6u16 => Self::AgaveBam,
            7u16 => Self::Sig,
            _ => Self::Unknown(client),
        }
    }
}

impl TryFrom<ClientId> for u16 {
    type Error = String;

    fn try_from(client: ClientId) -> Result<Self, Self::Error> {
        match client {
            ClientId::SolanaLabs => Ok(0u16),
            ClientId::JitoLabs => Ok(1u16),
            ClientId::Frankendancer => Ok(2u16),
            ClientId::Agave => Ok(3u16),
            ClientId::AgavePaladin => Ok(4u16),
            ClientId::Firedancer => Ok(5u16),
            ClientId::AgaveBam => Ok(6u16),
            ClientId::Sig => Ok(7u16),
            ClientId::Unknown(client @ 0u16..=7u16) => Err(format!("Invalid client: {client}")),
            ClientId::Unknown(client) => Ok(client),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_client_id() {
        assert_eq!(ClientId::from(0u16), ClientId::SolanaLabs);
        assert_eq!(ClientId::from(1u16), ClientId::JitoLabs);
        assert_eq!(ClientId::from(2u16), ClientId::Frankendancer);
        assert_eq!(ClientId::from(3u16), ClientId::Agave);
        assert_eq!(ClientId::from(4u16), ClientId::AgavePaladin);
        assert_eq!(ClientId::from(5u16), ClientId::Firedancer);
        assert_eq!(ClientId::from(6u16), ClientId::AgaveBam);
        assert_eq!(ClientId::from(7u16), ClientId::Sig);
        for client in 8u16..=u16::MAX {
            assert_eq!(ClientId::from(client), ClientId::Unknown(client));
        }
        assert_eq!(u16::try_from(ClientId::SolanaLabs), Ok(0u16));
        assert_eq!(u16::try_from(ClientId::JitoLabs), Ok(1u16));
        assert_eq!(u16::try_from(ClientId::Frankendancer), Ok(2u16));
        assert_eq!(u16::try_from(ClientId::Agave), Ok(3u16));
        assert_eq!(u16::try_from(ClientId::AgavePaladin), Ok(4u16));
        assert_eq!(u16::try_from(ClientId::Firedancer), Ok(5u16));
        assert_eq!(u16::try_from(ClientId::AgaveBam), Ok(6u16));
        assert_eq!(u16::try_from(ClientId::Sig), Ok(7u16));
        for client in 0..=7u16 {
            assert_eq!(
                u16::try_from(ClientId::Unknown(client)),
                Err(format!("Invalid client: {client}"))
            );
        }
        for client in 8u16..=u16::MAX {
            assert_eq!(u16::try_from(ClientId::Unknown(client)), Ok(client));
        }
    }
}
