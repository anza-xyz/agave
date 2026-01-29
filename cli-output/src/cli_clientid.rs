use {
    serde::{Deserialize, Deserializer, Serialize, Serializer},
    std::{fmt, str::FromStr},
};

#[derive(Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone)]
pub struct CliClientId(Option<u16>);

impl CliClientId {
    pub fn new(id: Option<u16>) -> Self {
        Self(id)
    }

    pub fn unknown() -> Self {
        Self(None)
    }
}

impl fmt::Display for CliClientId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Some(id) => write!(f, "{id}"),
            None => write!(f, "unknown"),
        }
    }
}

impl From<Option<u16>> for CliClientId {
    fn from(id: Option<u16>) -> Self {
        Self(id)
    }
}

impl FromStr for CliClientId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "unknown" {
            Ok(CliClientId(None))
        } else {
            Ok(CliClientId(Some(s.parse()?)))
        }
    }
}

impl Serialize for CliClientId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CliClientId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: &str = Deserialize::deserialize(deserializer)?;
        CliClientId::from_str(s).map_err(serde::de::Error::custom)
    }
}
