#[deprecated(
    since = "2.2.0",
    note = "Use solana-address-lookup-table interface instead"
)]
pub use solana_address_lookup_table_interface::{error, instruction, program, state};
pub use solana_message::AddressLookupTableAccount;
