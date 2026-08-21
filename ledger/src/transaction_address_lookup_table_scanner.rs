use {
    agave_reserved_account_keys::ReservedAccountKeys,
    agave_transaction_view::{
        transaction_data::TransactionData, transaction_view::SanitizedTransactionView,
    },
    solana_address_lookup_table_interface::{
        self as address_lookup_table, instruction::ProgramInstruction,
    },
    solana_pubkey::Pubkey,
    std::collections::HashSet,
    wincode::deserialize,
};

static RESERVED_IDS_SET: std::sync::LazyLock<HashSet<Pubkey>> =
    std::sync::LazyLock::new(|| ReservedAccountKeys::new_all_activated().active);

pub struct ScannedLookupTableExtensions {
    pub possibly_incomplete: bool,
    pub accounts: Vec<Pubkey>, // empty if no extensions found
}

pub fn scan_transaction<D>(
    transaction: &SanitizedTransactionView<D>,
) -> ScannedLookupTableExtensions
where
    D: TransactionData,
{
    // Accumulate accounts from account lookup table extension instructions
    let mut accounts = Vec::new();
    let mut no_user_programs = true;
    for (program_id, instruction) in transaction.program_instructions_iter() {
        if address_lookup_table::program::check_id(program_id) {
            if let Ok(ProgramInstruction::ExtendLookupTable { new_addresses }) =
                deserialize::<ProgramInstruction>(instruction.data)
            {
                accounts.extend(new_addresses);
            }
        } else {
            no_user_programs &= RESERVED_IDS_SET.contains(program_id);
        }
    }

    ScannedLookupTableExtensions {
        possibly_incomplete: !no_user_programs,
        accounts,
    }
}
