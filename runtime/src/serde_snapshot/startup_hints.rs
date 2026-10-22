use {
    crate::{bank::Bank, serde_snapshot::deserialize_wincode_from},
    log::*,
    wincode::{ReadError, ReadResult, SchemaRead, SchemaWrite, io::Reader},
};

/// A single startup tuning hint.
///
/// Hints are identified by tag, so extend by adding variants; never change an existing one.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub enum StartupHint {
    /// Number of accounts in the snapshot's bank.
    NumAccounts(u64),
}

/// The startup hints of a bank snapshot.
///
/// Written next to the bank snapshot on graceful exit, read back when starting from fastboot
/// state. Local-only (never archived) and wholly optional: every component must still initialize
/// correctly without it.
#[derive(Clone, Debug, Default, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct StartupHints {
    hints: Vec<StartupHint>,
}

impl StartupHints {
    pub fn new_from_bank(bank: &Bank) -> Self {
        Self::from_hints(vec![StartupHint::NumAccounts(
            bank.rc.accounts.accounts_db.accounts_index.num_accounts() as u64,
        )])
    }

    pub(crate) fn from_hints(hints: Vec<StartupHint>) -> Self {
        Self { hints }
    }

    /// `None` when the hints came from a newer version: wincode cannot read past a tag it does
    /// not know, so the whole lot is dropped rather than partially decoded.
    pub(crate) fn read_from<'a>(reader: impl Reader<'a>) -> ReadResult<Option<Self>> {
        match deserialize_wincode_from(reader) {
            Ok(startup_hints) => Ok(Some(startup_hints)),
            Err(ReadError::InvalidTagEncoding(tag)) => {
                warn!("ignoring startup hints: unknown hint {tag}");
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    // not `map().next()`: the match stops being irrefutable once a second variant exists
    #[allow(clippy::unnecessary_find_map)]
    pub fn num_accounts(&self) -> Option<u64> {
        self.hints.iter().find_map(|hint| match hint {
            StartupHint::NumAccounts(num_accounts) => Some(*num_accounts),
        })
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::serde_snapshot::serialize_into};

    fn roundtrip(hints: &StartupHints) -> StartupHints {
        let mut buf = Vec::new();
        serialize_into(&mut buf, hints).unwrap();
        StartupHints::read_from(buf.as_slice()).unwrap().unwrap()
    }

    #[test]
    fn test_roundtrip_startup_hints() {
        let hints = StartupHints::from_hints(vec![StartupHint::NumAccounts(1_234_567)]);
        let decoded = roundtrip(&hints);
        assert_eq!(decoded, hints);
        assert_eq!(decoded.num_accounts(), Some(1_234_567));
    }

    #[test]
    fn test_read_unknown_hint() {
        let mut encoded = 1u64.to_le_bytes().to_vec(); // one hint
        encoded.extend_from_slice(&u32::MAX.to_le_bytes()); // that no variant claims
        encoded.extend_from_slice(&[0xAB; 16]);

        assert!(
            StartupHints::read_from(encoded.as_slice())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_roundtrip_startup_hints_empty() {
        let hints = StartupHints::default();
        let decoded = roundtrip(&hints);
        assert_eq!(decoded, hints);
        assert_eq!(decoded.num_accounts(), None);
    }
}
