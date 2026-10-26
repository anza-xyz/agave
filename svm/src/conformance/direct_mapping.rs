//! Direct-mapping output adjustment shared by conformance harnesses.

/// Due to how Firedancer's VM CU accounting works, when
/// `virtual_address_space_adjustments` is enabled and execution fails with the
/// CU meter exhausted, we cannot compare the data region of the accounts with
/// Agave. Clears each supplied account's data hash in that case.
#[cfg(feature = "conformance")]
pub fn direct_mapping_handle_cu_exhaustion<'a>(
    virtual_address_space_adjustments_active: bool,
    cu_avail: u64,
    has_err: bool,
    accounts: impl IntoIterator<Item = &'a mut protosol::protos::AcctState>,
) {
    if virtual_address_space_adjustments_active && cu_avail == 0 && has_err {
        for account in accounts {
            account.set_data_hash(0);
        }
    }
}
