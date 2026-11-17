//! The ABI between the program runtime and an executing program.

use {
    solana_program_entrypoint::{BPF_ALIGN_OF_U128, MAX_PERMITTED_DATA_INCREASE},
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_deprecated,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ABI {
    /// Program Runtime ABI V0.
    ///
    /// The legacy ABI, supported only for programs owned by the deprecated
    /// Loader V1. Input is unaligned and accounts cannot be reallocated.
    V0,
    /// Program Runtime ABI V1.
    ///
    /// The current ABI, supported for programs owned by the deprecated Loader
    /// V2 and the upgradeable Loader V3. Input is aligned to
    /// [`BPF_ALIGN_OF_U128`] (8 bytes) and each account reserves
    /// [`MAX_PERMITTED_DATA_INCREASE`] bytes past its data for reallocations.
    V1,
}

impl ABI {
    /// Resolves the ABI of a program from the owner of its program account.
    pub fn resolve(program_owner: &Pubkey) -> Self {
        if bpf_loader_deprecated::check_id(program_owner) {
            Self::V0
        } else {
            Self::V1
        }
    }

    /// The alignment of the program's input and heap allocations.
    pub const fn alignment(self) -> usize {
        match self {
            Self::V0 => align_of::<u8>(),
            Self::V1 => BPF_ALIGN_OF_U128,
        }
    }

    /// The address space reserved past an account's data for it to grow into.
    pub const fn account_realloc_padding(self) -> usize {
        match self {
            Self::V0 => 0,
            Self::V1 => MAX_PERMITTED_DATA_INCREASE,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable, loader_v4, native_loader},
        test_case::test_case,
    };

    #[test_case(bpf_loader_deprecated::id(), ABI::V0; "loader_v1")]
    #[test_case(bpf_loader::id(), ABI::V1; "loader_v2")]
    #[test_case(bpf_loader_upgradeable::id(), ABI::V1; "loader_v3")]
    #[test_case(loader_v4::id(), ABI::V1; "loader_v4")]
    #[test_case(native_loader::id(), ABI::V1; "native_loader")]
    fn test_resolve(program_owner: Pubkey, expected: ABI) {
        assert_eq!(ABI::resolve(&program_owner), expected);
    }

    #[test]
    fn test_v0_properties() {
        assert_eq!(ABI::V0.alignment(), 1);
        assert_eq!(ABI::V0.account_realloc_padding(), 0);
    }

    #[test]
    fn test_v1_properties() {
        assert_eq!(ABI::V1.alignment(), BPF_ALIGN_OF_U128);
        assert_eq!(
            ABI::V1.account_realloc_padding(),
            MAX_PERMITTED_DATA_INCREASE,
        );
    }
}
