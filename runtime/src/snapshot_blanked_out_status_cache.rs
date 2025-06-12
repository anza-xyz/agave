use {
    crate::{bank::BankSlotDelta, status_cache::Status},
    std::sync::{Arc, Mutex},
};

/// It is very difficult to change the format of `TransactionError` because it gets written to disk
/// when the validator snapshots `BankStatusCache`. The thing is, `BankStatusCache` has no need of
/// `TransactionErrors` or indeed `TransactionResults` at all; all it needs to track is whether a
/// given transaction message has been seen or not.
///
/// This type exists to wrap a `BankSlotDelta` in such a way that unconditionally drops any
/// `TransactionErrors` and replaces them with `OK(())`.
pub struct BankSlotDeltaWithAlwaysOkTransactionResult(BankSlotDelta);

fn status_with_replaced_inner_type<T: Clone, R: Clone>(
    status: Status<T>,
    replacement: R,
) -> Status<R> {
    let status_guard = status.lock().unwrap();
    let replaced = status_guard
        .clone()
        .into_iter()
        .map(|(hash, (max_slot, statuses))| {
            (
                hash,
                (
                    max_slot,
                    statuses
                        .into_iter()
                        .map(|(key_slice, _transaction_status)| (key_slice, replacement.clone()))
                        .collect(),
                ),
            )
        })
        .collect();
    Arc::new(Mutex::new(replaced))
}

impl From<BankSlotDeltaWithAlwaysOkTransactionResult> for BankSlotDelta {
    fn from(value: BankSlotDeltaWithAlwaysOkTransactionResult) -> Self {
        value.0
    }
}

impl From<BankSlotDelta> for BankSlotDeltaWithAlwaysOkTransactionResult {
    fn from(value: BankSlotDelta) -> Self {
        let (slot, is_root, status) = value;
        let status_with_result_blanked_out = status_with_replaced_inner_type(
            status,
            // Drop the `Result` here; replace it with `Ok(())`.
            Ok(()),
        );
        BankSlotDeltaWithAlwaysOkTransactionResult((slot, is_root, status_with_result_blanked_out))
    }
}
