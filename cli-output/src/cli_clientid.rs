use {
    serde::{Deserialize, Serialize},
    std::fmt,
};

#[derive(Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
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

impl From<CliClientId> for String {
    fn from(id: CliClientId) -> String {
        id.to_string()
    }
}

impl From<String> for CliClientId {
    fn from(s: String) -> Self {
        if s == "Unknown" {
            Self(None)
        } else {
            Self(Some(s))
        }
    }
}
