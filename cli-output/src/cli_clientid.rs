use {
    serde::{Deserialize, Deserializer, Serialize, Serializer},
    std::fmt,
};

#[derive(Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone)]
pub struct CliClientId(Option<String>);

impl CliClientId {
    pub fn unknown() -> Self {
        Self(None)
    }
}

impl fmt::Display for CliClientId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.0 {
            Some(id) => write!(f, "{id}"),
            None => write!(f, "Unknown"),
        }
    }
}

impl From<Option<String>> for CliClientId {
    fn from(id: Option<String>) -> Self {
        Self(id)
    }
}

impl Serialize for CliClientId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialize as a plain string so it can be a JSON map key
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CliClientId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == "Unknown" {
            Ok(Self(None))
        } else {
            Ok(Self(Some(s)))
        }
    }
}
