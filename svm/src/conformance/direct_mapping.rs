//! Direct-mapping output adjustment shared by conformance harnesses.

/// Due to how Firedancer's VM CU accounting works, when
/// `virtual_address_space_adjustments` is enabled and execution fails with the
/// CU meter exhausted, we cannot compare the data region of the accounts with
/// Agave. Clears each supplied data buffer in that case.
pub fn direct_mapping_handle_cu_exhaustion<'a>(
    virtual_address_space_adjustments_active: bool,
    cu_avail: u64,
    has_err: bool,
    account_data: impl IntoIterator<Item = &'a mut Vec<u8>>,
) {
    if virtual_address_space_adjustments_active && cu_avail == 0 && has_err {
        for data in account_data {
            data.clear();
        }
    }
}
