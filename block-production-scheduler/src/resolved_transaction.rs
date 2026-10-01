use {
    agave_scheduling_utils::{
        pubkeys_ptr::{OwnedPubkeysPtr, PubkeysPtr},
        transaction_ptr::{OwnedTransactionPtr, TransactionPtr},
    },
    agave_transaction_view::{
        resolved_transaction_view::ResolvedTransactionView, result::TransactionViewError,
        transaction_view::SanitizedTransactionView,
    },
    core::ptr::NonNull,
    rts_alloc::Allocator,
    solana_message::v0::LoadedAddressesView,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::collections::HashSet,
};

pub(super) struct ResolvedPubkeys {
    pubkeys: Option<PubkeysPtr>,
    num_writable: usize,
}

impl<'a> From<&'a ResolvedPubkeys> for LoadedAddressesView<'a> {
    fn from(pubkeys: &'a ResolvedPubkeys) -> Self {
        let keys = pubkeys
            .pubkeys
            .as_ref()
            .map_or(&[][..], PubkeysPtr::as_slice);
        let (writable, readonly) = keys.split_at(pubkeys.num_writable);
        Self { writable, readonly }
    }
}

/// A resolved view backed by shared allocations, released explicitly through `free`.
pub(super) struct ResolvedTransaction {
    transaction: TransactionPtr,
    resolved_pubkeys: Option<PubkeysPtr>,
    pub(super) view: ResolvedTransactionView<TransactionPtr, ResolvedPubkeys>,
}

impl ResolvedTransaction {
    /// Consumes both allocations, freeing them if construction fails.
    ///
    /// # Safety
    /// The transaction must belong to `allocator`.
    pub(super) unsafe fn try_new(
        transaction: OwnedTransactionPtr<'_>,
        resolved_pubkeys: OwnedPubkeysPtr<'_>,
        allocator: &Allocator,
        reserved_account_keys: &HashSet<Pubkey, PubkeyHasherBuilder>,
    ) -> Result<Self, TransactionViewError> {
        // SAFETY: the caller supplies the transaction's allocator. The guard retains ownership
        // throughout construction; this alias is never freed separately.
        let data = unsafe {
            TransactionPtr::from_sharable_transaction_region(&transaction.to_region(), allocator)
        };
        let view = SanitizedTransactionView::try_new_sanitized(data)?;
        let num_writable = usize::from(view.total_writable_lookup_accounts());
        let keys = resolved_pubkeys.as_slice();
        if num_writable > keys.len() {
            return Err(TransactionViewError::AddressLookupMismatch);
        }
        let pubkeys = (!keys.is_empty()).then(|| {
            let ptr = NonNull::from(keys).cast();
            // SAFETY: the pointer is derived from the entire slice. The guard keeps it alive,
            // and this alias is never freed separately from the owned pointer retained below.
            unsafe { PubkeysPtr::from_raw_parts(ptr, keys.len()) }
        });
        let addresses = ResolvedPubkeys {
            pubkeys,
            num_writable,
        };
        let view = ResolvedTransactionView::try_new_with_source(
            view,
            Some(addresses),
            reserved_account_keys,
        )?;
        Ok(Self {
            transaction: transaction.into_inner(),
            resolved_pubkeys: resolved_pubkeys.into_inner(),
            view,
        })
    }

    /// # Safety
    /// Both allocations must still be exclusively owned and belong to `allocator`.
    pub(super) unsafe fn free(self, allocator: &Allocator) {
        drop(self.view);
        // SAFETY: the caller owns the transaction and the view no longer references it.
        unsafe { self.transaction.free(allocator) };
        if let Some(pubkeys) = self.resolved_pubkeys {
            // SAFETY: the caller owns the pubkey allocation and the view no longer references it.
            unsafe { pubkeys.free(allocator) };
        }
    }
}
