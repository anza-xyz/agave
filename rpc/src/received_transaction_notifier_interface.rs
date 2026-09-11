use {solana_clock::Slot, solana_signature::Signature, std::sync::Arc};

/// Notifier for transactions received on the RPC `sendTransaction` path.
///
/// Invoked by the RPC request processor at the point a transaction is admitted to the
/// send transaction service, after the hand-off and before any forwarding to the current
/// leader has been attempted. No execution has occurred at this point.
///
/// Implementations are called synchronously on an RPC request thread and must never
/// block; see `GeyserPlugin::notify_transaction_received`.
pub trait ReceivedTransactionNotifier {
    fn notify_transaction_received(
        &self,
        signature: &Signature,
        wire_transaction: &[u8],
        received_ns: u64,
        slot_hint: Slot,
        preflight_skipped: bool,
    );
}

pub type ReceivedTransactionNotifierArc = Arc<dyn ReceivedTransactionNotifier + Sync + Send>;
