use crate::{transaction_data::TransactionData, transaction_view::TransactionView};

pub struct SanitizeError;

pub fn sanitize(view: &TransactionView<impl TransactionData>) -> Result<(), SanitizeError> {
    sanitize_signatures(view)?;
    sanitize_account_access(view)?;
    sanitize_instructions(view)?;
    sanitize_address_table_lookups(view)?;

    Ok(())
}

fn sanitize_signatures(view: &TransactionView<impl TransactionData>) -> Result<(), SanitizeError> {
    // Check the required number of signatures matches the number of signatures.
    if view.num_signatures() != view.num_required_signatures() {
        return Err(SanitizeError);
    }

    // Each signature is associated with a unique static public key.
    // Check that there are at least as many static account keys as signatures.
    if view.num_static_account_keys() < view.num_signatures() {
        return Err(SanitizeError);
    }

    Ok(())
}

fn sanitize_account_access(
    view: &TransactionView<impl TransactionData>,
) -> Result<(), SanitizeError> {
    // Check there is no overlap of signing area and readonly non-signing area.
    // We have already checked that `num_required_signatures` is less than or equal to `num_static_account_keys`,
    // so it is safe to use wrapping arithmetic.
    if view.num_readonly_unsigned_accounts()
        > view
            .num_static_account_keys()
            .wrapping_sub(view.num_required_signatures())
    {
        return Err(SanitizeError);
    }

    // Check there is at least 1 writable fee-payer account.
    if view.num_readonly_signed_accounts() >= view.num_required_signatures() {
        return Err(SanitizeError);
    }

    // Check there are not more than 256 accounts.
    if total_number_of_accounts(view) > 256 {
        return Err(SanitizeError);
    }

    Ok(())
}

fn sanitize_instructions(
    view: &TransactionView<impl TransactionData>,
) -> Result<(), SanitizeError> {
    // already verified there is at least one static account.
    let max_program_id_index = view.num_static_account_keys().wrapping_sub(1);
    // verified that there are no more than 256 accounts in `sanitize_account_access`
    let max_account_index = total_number_of_accounts(view).wrapping_sub(1) as u8;

    for instruction in view.instructions_iter() {
        // Check that program indexes are static account keys.
        if instruction.program_id_index > max_program_id_index {
            return Err(SanitizeError);
        }

        // Check that the program index is not the fee-payer.
        if instruction.program_id_index == 0 {
            return Err(SanitizeError);
        }

        // Check that all account indexes are valid.
        for account_index in instruction.accounts.iter().copied() {
            if account_index > max_account_index {
                return Err(SanitizeError);
            }
        }
    }

    Ok(())
}

fn sanitize_address_table_lookups(
    view: &TransactionView<impl TransactionData>,
) -> Result<(), SanitizeError> {
    for address_table_lookup in view.address_table_lookup_iter() {
        // Check that there is at least one account lookup.
        if address_table_lookup.writable_indexes.is_empty()
            && address_table_lookup.readonly_indexes.is_empty()
        {
            return Err(SanitizeError);
        }
    }

    Ok(())
}

fn total_number_of_accounts(view: &TransactionView<impl TransactionData>) -> u16 {
    u16::from(view.num_static_account_keys())
        .saturating_add(view.total_writable_lookup_accounts())
        .saturating_add(view.total_readonly_lookup_accounts())
}
