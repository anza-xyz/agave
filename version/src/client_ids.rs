use {
    num_enum::{FromPrimitive, IntoPrimitive},
    strum::Display,
};

#[derive(Clone, Debug, Display, Eq, FromPrimitive, IntoPrimitive, PartialEq)]
#[repr(u16)]
pub enum ClientId {
    SolanaLabs = 0,
    JitoLabs = 1,
    Frankendancer = 2,
    Agave = 3,
    AgavePaladin = 4,
    Firedancer = 5,
    AgaveBam = 6,
    Sig = 7,
    Rakurai = 8,
    HarmonicFiredancer = 9,
    HarmonicAgave = 10,
    HarmonicFrankendancer = 11,
    FireBAM = 12,
    Raiku = 13,
    #[num_enum(catch_all)]
    #[strum(to_string = "Unknown({0})")]
    Unknown(u16),
}

impl ClientId {
    pub const fn this_client() -> Self {
        // Other client implementations need to modify this line.
        Self::Agave
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
        assert_eq!(ClientId::from(8u16), ClientId::Rakurai);
        assert_eq!(ClientId::from(9u16), ClientId::HarmonicFiredancer);
        assert_eq!(ClientId::from(10u16), ClientId::HarmonicAgave);
        assert_eq!(ClientId::from(11u16), ClientId::HarmonicFrankendancer);
        assert_eq!(ClientId::from(12u16), ClientId::FireBAM);
        assert_eq!(ClientId::from(13u16), ClientId::Raiku);
        for client in 14u16..=u16::MAX {
            assert_eq!(ClientId::from(client), ClientId::Unknown(client));
        }
        assert_eq!(u16::from(ClientId::SolanaLabs), 0u16);
        assert_eq!(u16::from(ClientId::JitoLabs), 1u16);
        assert_eq!(u16::from(ClientId::Frankendancer), 2u16);
        assert_eq!(u16::from(ClientId::Agave), 3u16);
        assert_eq!(u16::from(ClientId::AgavePaladin), 4u16);
        assert_eq!(u16::from(ClientId::Firedancer), 5u16);
        assert_eq!(u16::from(ClientId::AgaveBam), 6u16);
        assert_eq!(u16::from(ClientId::Sig), 7u16);
        assert_eq!(u16::from(ClientId::Rakurai), 8u16);
        assert_eq!(u16::from(ClientId::HarmonicFiredancer), 9u16);
        assert_eq!(u16::from(ClientId::HarmonicAgave), 10u16);
        assert_eq!(u16::from(ClientId::HarmonicFrankendancer), 11u16);
        assert_eq!(u16::from(ClientId::FireBAM), 12u16);
        assert_eq!(u16::from(ClientId::Raiku), 13u16);

        for client in 0u16..=u16::MAX {
            assert_eq!(u16::from(ClientId::Unknown(client)), client);
        }
    }

    #[test]
    fn test_fmt() {
        assert_eq!(format!("{}", ClientId::SolanaLabs), "SolanaLabs");
        assert_eq!(format!("{}", ClientId::JitoLabs), "JitoLabs");
        assert_eq!(format!("{}", ClientId::Frankendancer), "Frankendancer");
        assert_eq!(format!("{}", ClientId::Agave), "Agave");
        assert_eq!(format!("{}", ClientId::AgavePaladin), "AgavePaladin");
        assert_eq!(format!("{}", ClientId::Firedancer), "Firedancer");
        assert_eq!(format!("{}", ClientId::AgaveBam), "AgaveBam");
        assert_eq!(format!("{}", ClientId::Sig), "Sig");
        assert_eq!(format!("{}", ClientId::Rakurai), "Rakurai");
        assert_eq!(
            format!("{}", ClientId::HarmonicFiredancer),
            "HarmonicFiredancer"
        );
        assert_eq!(format!("{}", ClientId::HarmonicAgave), "HarmonicAgave");
        assert_eq!(
            format!("{}", ClientId::HarmonicFrankendancer),
            "HarmonicFrankendancer"
        );
        assert_eq!(format!("{}", ClientId::FireBAM), "FireBAM");
        assert_eq!(format!("{}", ClientId::Raiku), "Raiku");
        assert_eq!(format!("{}", ClientId::Unknown(0)), "Unknown(0)");
        assert_eq!(format!("{}", ClientId::Unknown(u16::MAX)), "Unknown(65535)");
    }
}
