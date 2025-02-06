use core::fmt;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum AddressLookupError {
    /// Attempted to lookup addresses from a table that does not exist
    LookupTableAccountNotFound,

    /// Attempted to lookup addresses from an account owned by the wrong program
    InvalidAccountOwner,

    /// Attempted to lookup addresses from an invalid account
    InvalidAccountData,

    /// Address lookup contains an invalid index
    InvalidLookupIndex,
}

impl std::error::Error for AddressLookupError {}

impl fmt::Display for AddressLookupError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Self::LookupTableAccountNotFound => {
                "Attempted to lookup addresses from a table that does not exist"
            }
            Self::InvalidAccountOwner => {
                "Attempted to lookup addresses from an account owned by the wrong program"
            }
            Self::InvalidAccountData => "Attempted to lookup addresses from an invalid account",
            Self::InvalidLookupIndex => "Address lookup contains an invalid index",
        })
    }
}
