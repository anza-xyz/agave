use solana_pubkey::Pubkey;

#[repr(u64)]
#[cfg_attr(feature = "frozen-abi", derive(solana_frozen_abi_macro::AbiExample))]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LoaderV4Status {
    /// Program is in maintenance
    Retracted,
    /// Program is ready to be executed
    Deployed,
    /// Same as `Deployed`, but can not be retracted anymore
    Finalized,
}

/// LoaderV4 account states
#[repr(C)]
#[cfg_attr(feature = "frozen-abi", derive(solana_frozen_abi_macro::AbiExample))]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct LoaderV4State {
    /// Slot in which the program was last deployed, retracted or initialized.
    pub slot: u64,
    /// Address of signer which can send program management instructions when the status is not finalized.
    /// Otherwise a forwarding to the next version of the finalized program.
    pub authority_address_or_next_version: Pubkey,
    /// Deployment status.
    pub status: LoaderV4Status,
    // The raw program data follows this serialized structure in the
    // account's data.
}

impl LoaderV4State {
    /// Size of a serialized program account.
    pub const fn program_data_offset() -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use {super::*, memoffset::offset_of};

    #[test]
    fn test_layout() {
        assert_eq!(offset_of!(LoaderV4State, slot), 0x00);
        assert_eq!(
            offset_of!(LoaderV4State, authority_address_or_next_version),
            0x08
        );
        assert_eq!(offset_of!(LoaderV4State, status), 0x28);
        assert_eq!(LoaderV4State::program_data_offset(), 0x30);
    }
}
