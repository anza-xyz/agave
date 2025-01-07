//! Instruction account.

use {crate::proto::InstrAcct as ProtoInstrAccount, solana_keccak_hasher::Hasher};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InstrAccount {
    /// The index of this account in the fixture's accounts list.
    pub index: u32,
    /// Whether or not this account is a signer.
    pub is_signer: bool,
    /// Whether or not this account is writable.
    pub is_writable: bool,
}

impl From<ProtoInstrAccount> for InstrAccount {
    fn from(value: ProtoInstrAccount) -> Self {
        let ProtoInstrAccount {
            index,
            is_writable,
            is_signer,
        } = value;
        Self {
            index,
            is_signer,
            is_writable,
        }
    }
}

impl From<InstrAccount> for ProtoInstrAccount {
    fn from(value: InstrAccount) -> Self {
        let InstrAccount {
            index,
            is_signer,
            is_writable,
        } = value;
        Self {
            index,
            is_signer,
            is_writable,
        }
    }
}

pub(crate) fn hash_proto_instr_accounts(hasher: &mut Hasher, instr_accounts: &[ProtoInstrAccount]) {
    for account in instr_accounts {
        hasher.hash(&account.index.to_le_bytes());
        hasher.hash(&[account.is_signer as u8]);
        hasher.hash(&[account.is_writable as u8]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_to_from_instr_account(instr_account: InstrAccount) {
        let proto = ProtoInstrAccount::from(instr_account.clone());
        let converted_instr_account = InstrAccount::from(proto);
        assert_eq!(instr_account, converted_instr_account);
    }

    #[test]
    fn test_conversion_instr_account() {
        let instr_account = InstrAccount {
            index: 0,
            is_signer: false,
            is_writable: false,
        };
        test_to_from_instr_account(instr_account);

        let instr_account = InstrAccount {
            index: 10,
            is_signer: true,
            is_writable: true,
        };
        test_to_from_instr_account(instr_account);

        let instr_account = InstrAccount {
            index: u32::MAX,
            is_signer: true,
            is_writable: false,
        };
        test_to_from_instr_account(instr_account);
    }
}
