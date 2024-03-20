//! The account meta and related structs for the tiered storage.

use {
    crate::{accounts_index::ZeroLamport, tiered_storage::owners::OwnerOffset},
    bytemuck::{Pod, Zeroable},
    modular_bitfield::prelude::*,
    solana_sdk::stake_history::Epoch,
};

/// The number of bits used in lamports_info field.
/// Note that this value must match the bits in AccountMetaFlags::lamports_info.
pub const LAMPORTS_INFO_BITS: u64 = 30;
/// The max lamports balance that the lamports_info field can handle.
/// Any lamports beyond this value will be stored separately in optional fields.
pub const LAMPORTS_INFO_MAX_BALANCE: u64 =
    ((1u64 << LAMPORTS_INFO_BITS) - 1) - LAMPORTS_INFO_RESERVED_VALUES;

/// The number of special values inside lamports_info.
/// This const MUST be updated when adding new reserved values.
pub const LAMPORTS_INFO_RESERVED_VALUES: u64 = 2;

/// A reserved lamports_info value indicating zero-lamports balance.
pub const LAMPORTS_INFO_IS_ZERO_BALANCE: u32 = 0;
/// A reserved lamports_info value indicating the lamports balance is stored
/// in optional fields.
pub const LAMPORTS_INFO_HAS_OPTIONAL_FIELD: u32 = 1;

/// The struct that handles the account meta flags.
#[bitfield(bits = 32)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Pod, Zeroable)]
pub struct AccountMetaFlags {
    /// whether the account meta has rent epoch
    pub has_rent_epoch: bool,
    /// whether the account is executable
    pub executable: bool,
    /// this fewer-than-u64 lamports info stores lamports that can fit
    /// within its limitation, or a bit indicating the lamport is stored
    /// separately as an optional field.
    ///
    /// Note that the number of bits using in this field must match
    /// the const LAMPORTS_INFO_BITS.
    pub lamports_info: B30,
}

// Ensure there are no implicit padding bytes
const _: () = assert!(std::mem::size_of::<AccountMetaFlags>() == 4);

/// A trait that allows different implementations of the account meta that
/// support different tiers of the accounts storage.
pub trait TieredAccountMeta: Sized + ZeroLamport {
    /// Constructs a TieredAcountMeta instance.
    fn new() -> Self;

    /// A builder function that initializes the number of padding bytes
    /// for the account data associated with the current meta.
    fn with_account_data_padding(self, padding: u8) -> Self;

    /// A builder function that initializes the owner offset.
    fn with_owner_offset(self, owner_offset: OwnerOffset) -> Self;

    /// A builder function that initializes the account data size.
    /// The size here represents the logical data size without compression.
    fn with_account_data_size(self, account_data_size: u64) -> Self;

    /// A builder function that initializes the AccountMetaFlags of the current
    /// meta.
    fn with_flags(self, flags: &AccountMetaFlags) -> Self;

    /// Returns the balance of the lamports associated with the account
    /// from the TieredAccountMeta, or None if the lamports is stored
    /// inside the optional field.
    fn lamports_from_meta(&self) -> Option<u64>;

    /// Returns the balance of the lamports associated with the account
    /// from the optional fields, or None if the lamports is stored
    /// inside the TieredAccountMeta.
    fn lamports_from_optional_fields(&self, _account_block: &[u8]) -> Option<u64>;

    /// Returns the number of padding bytes for the associated account data
    fn account_data_padding(&self) -> u8;

    /// Returns the offset to the accounts' owner in the current AccountsFile.
    fn owner_offset(&self) -> OwnerOffset;

    /// Returns the AccountMetaFlags of the current meta.
    fn flags(&self) -> &AccountMetaFlags;

    /// Returns true if the TieredAccountMeta implementation supports multiple
    /// accounts sharing one account block.
    fn supports_shared_account_block() -> bool;

    /// Returns the epoch that this account will next owe rent by parsing
    /// the specified account block.  None will be returned if this account
    /// does not persist this optional field.
    fn rent_epoch(&self, _account_block: &[u8]) -> Option<Epoch>;

    /// Returns the offset of the optional fields based on the specified account
    /// block.
    fn optional_fields_offset(&self, _account_block: &[u8]) -> usize;

    /// Returns the length of the data associated to this account based on the
    /// specified account block.
    fn account_data_size(&self, _account_block: &[u8]) -> usize;

    /// Returns the data associated to this account based on the specified
    /// account block.
    fn account_data<'a>(&self, _account_block: &'a [u8]) -> &'a [u8];
}

impl AccountMetaFlags {
    pub fn new_from(optional_fields: &AccountMetaOptionalFields, lamports: u64) -> Self {
        let mut flags = AccountMetaFlags::default();
        flags.set_has_rent_epoch(optional_fields.rent_epoch.is_some());
        if optional_fields.lamports.is_some() {
            flags.set_lamports_info(LAMPORTS_INFO_HAS_OPTIONAL_FIELD);
        } else if lamports != 0 {
            debug_assert!(lamports <= LAMPORTS_INFO_MAX_BALANCE);
            flags.set_lamports_info((lamports + LAMPORTS_INFO_RESERVED_VALUES) as u32);
        }
        flags.set_executable(false);
        flags
    }

    pub fn lamports(&self) -> Option<u64> {
        match self.lamports_info() {
            LAMPORTS_INFO_IS_ZERO_BALANCE => Some(0),
            LAMPORTS_INFO_HAS_OPTIONAL_FIELD => None,
            packed_lamports => Some(packed_lamports as u64 - LAMPORTS_INFO_RESERVED_VALUES),
        }
    }

    pub fn is_zero_lamport(&self) -> bool {
        self.lamports_info() == LAMPORTS_INFO_IS_ZERO_BALANCE
    }

    pub fn has_optional_lamports_field(&self) -> bool {
        self.lamports_info() == LAMPORTS_INFO_HAS_OPTIONAL_FIELD
    }

    pub fn get_optional_lamports_field(lamports: u64) -> Option<u64> {
        if lamports > LAMPORTS_INFO_MAX_BALANCE {
            Some(lamports)
        } else {
            None
        }
    }
}

/// The in-memory struct for the optional fields for tiered account meta.
///
/// Note that the storage representation of the optional fields might be
/// different from its in-memory representation.
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct AccountMetaOptionalFields {
    /// the epoch at which its associated account will next owe rent
    pub rent_epoch: Option<Epoch>,
    /// The balance of this account.
    ///
    /// It is Some only when lamports balance of the current account
    /// cannot be stored inside the AccountMeta.
    pub lamports: Option<u64>,
}

impl AccountMetaOptionalFields {
    /// The size of the optional fields in bytes (excluding the boolean flags).
    pub fn size(&self) -> usize {
        self.rent_epoch.map_or(0, |_| std::mem::size_of::<Epoch>())
            + self.lamports.map_or(0, |_| std::mem::size_of::<u64>())
    }

    /// Given the specified AccountMetaFlags, returns the size of its
    /// associated AccountMetaOptionalFields.
    pub fn size_from_flags(flags: &AccountMetaFlags) -> usize {
        let mut fields_size = 0;
        if flags.has_rent_epoch() {
            fields_size += std::mem::size_of::<Epoch>();
        }

        if flags.lamports_info() == LAMPORTS_INFO_HAS_OPTIONAL_FIELD {
            fields_size += std::mem::size_of::<u64>();
        }

        fields_size
    }

    /// Given the specified AccountMetaFlags, returns the relative offset
    /// of its rent_epoch field to the offset of its optional fields entry.
    pub fn rent_epoch_offset(_flags: &AccountMetaFlags) -> usize {
        0
    }

    /// Given the specified AccountMetaFlags, returns the relative offset
    /// of its lamports field to the offset of its optional fields entry.
    pub fn lamports_offset(flags: &AccountMetaFlags) -> usize {
        let mut offset = Self::rent_epoch_offset(flags);
        if flags.has_rent_epoch() {
            offset += std::mem::size_of::<Epoch>();
        }

        offset
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    impl AccountMetaFlags {
        pub fn new_from_test(optional_fields: &AccountMetaOptionalFields) -> Self {
            AccountMetaFlags::new_from(optional_fields, 0)
        }
    }

    #[test]
    fn test_account_meta_flags_new() {
        let flags = AccountMetaFlags::new();

        assert!(!flags.has_rent_epoch());
        assert_eq!(flags.lamports_info(), 0u32);

        assert_eq!(
            std::mem::size_of::<AccountMetaFlags>(),
            std::mem::size_of::<u32>()
        );
    }

    fn verify_flags_serialization(flags: &AccountMetaFlags) {
        assert_eq!(AccountMetaFlags::from_bytes(flags.into_bytes()), *flags);
    }

    #[test]
    fn test_account_meta_flags_set() {
        let mut flags = AccountMetaFlags::new();

        flags.set_has_rent_epoch(true);

        assert!(flags.has_rent_epoch());
        assert!(!flags.executable());
        verify_flags_serialization(&flags);

        flags.set_executable(true);
        assert!(flags.has_rent_epoch());
        assert!(flags.executable());
        verify_flags_serialization(&flags);

        // make sure the lamports_info bits are untouched.
        assert_eq!(flags.lamports_info(), 0u32);
    }

    fn update_and_verify_flags(opt_fields: &AccountMetaOptionalFields) {
        let flags: AccountMetaFlags = AccountMetaFlags::new_from_test(opt_fields);
        assert_eq!(flags.has_rent_epoch(), opt_fields.rent_epoch.is_some());
        assert_eq!(
            flags.lamports_info(),
            opt_fields
                .lamports
                .map_or(0, |_| LAMPORTS_INFO_HAS_OPTIONAL_FIELD)
        );
    }

    #[test]
    fn test_optional_fields_update_flags() {
        let test_epoch = 5432312;
        let test_lamports = 2314312321321;

        for rent_epoch in [None, Some(test_epoch)] {
            for lamports in [None, Some(test_lamports)] {
                update_and_verify_flags(&AccountMetaOptionalFields {
                    rent_epoch,
                    lamports,
                });
            }
        }
    }

    #[test]
    fn test_optional_fields_size() {
        let test_epoch = 5432312;
        let test_lamports = 2314312321321;

        for rent_epoch in [None, Some(test_epoch)] {
            for lamports in [None, Some(test_lamports)] {
                let opt_fields = AccountMetaOptionalFields {
                    rent_epoch,
                    lamports,
                };
                assert_eq!(
                    opt_fields.size(),
                    rent_epoch.map_or(0, |_| std::mem::size_of::<Epoch>())
                        + lamports.map_or(0, |_| std::mem::size_of::<u64>()),
                );
                assert_eq!(
                    opt_fields.size(),
                    AccountMetaOptionalFields::size_from_flags(&AccountMetaFlags::new_from_test(
                        &opt_fields,
                    ))
                );
            }
        }
    }

    #[test]
    fn test_optional_fields_offset() {
        let test_epoch = 5432312;
        let test_lamports = 2314312321321;

        for rent_epoch in [None, Some(test_epoch)] {
            for lamports in [None, Some(test_lamports)] {
                let opt_fields = AccountMetaOptionalFields {
                    rent_epoch,
                    lamports,
                };
                let flags = AccountMetaFlags::new_from_test(&opt_fields);
                assert_eq!(AccountMetaOptionalFields::rent_epoch_offset(&flags), 0,);
                assert_eq!(
                    AccountMetaOptionalFields::lamports_offset(&flags),
                    rent_epoch.map_or(0, |_| std::mem::size_of::<Epoch>()),
                );
                assert_eq!(
                    AccountMetaOptionalFields::size_from_flags(&flags),
                    rent_epoch.map_or(0, |_| std::mem::size_of::<Epoch>())
                        + lamports.map_or(0, |_| std::mem::size_of::<u64>()),
                );
            }
        }
    }
}
