/// The account key is a 32-byte array that represents a public key.
pub type AccountKey = *const u8;

/// An opaque pointer to a transaction. This will be provided to any plugin and
/// should always be non-null. The plugin should not pass any pointers to any
/// functions on the [`TransactionInterface`] that are not provided.
pub type TransactionPtr = *const core::ffi::c_void;

/// Returns the number of signatures in this transaction.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionNumSignaturesFn =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr) -> usize;

/// Returns pointer to the first signature in the transaction.
/// The returned pointer is valid for the lifetime of the transaction, and is
/// guaranteed to be non-null and 64 bytes long.
/// The number of signatures in the transaction can be obtained by calling
/// [`TransactionNumSignaturesFn`].
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionSignaturesFn =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr) -> *const u8;

/// Returns the total number of signatures in the transaction, including any
/// pre-compile signatures.
/// WARNING: This function should not be used to determine the number of
/// signatures returned by `TransactionSignaturesFn`. Instead, use
/// `TransactionNumSignaturesFn`.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionNumTotalSignatures =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr) -> u64;

/// Returns the number of requested write-locks in this transaction.
/// This does not consider if write-locks are demoted.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionNumWriteLocksFn = unsafe extern "C" fn(transaction_ptr: TransactionPtr) -> u64;

/// Returns a reference to the transaction's recent blockhash.
/// The returned pointer is valid for the lifetime of the transaction, and is
/// guaranteed to be non-null and 32 bytes long.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionRecentBlockhashFn =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr) -> *const u8;

/// Return the number of instructions in the transaction.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionNumInstructionsFn =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr) -> usize;

/// A C-compatible non-owning reference to an instruction in a transaction.
#[repr(C)]
pub struct Instruction {
    /// The program index of the instruction.
    pub program_index: u8,
    /// The number of accounts used by the instruction.
    pub num_accounts: u16,
    /// Pointer to the first account index used by the instruction.
    /// Guaranteed to be non-null and valid for the lifetime of a transaction.
    pub accounts: *const u8,
    /// The number of data bytes used by the instruction.
    pub data_len: u16,
    /// Pointer to the first data byte used by the instruction.
    /// Guaranteed to be non-null and valid for the lifetime of a transaction.
    pub data: *const u8,
}

/// A C-compatible interface that can be used to interact with instructons in
/// transactions. This callback interface is used to inspect instructions in a
/// loop and optionally break early.
#[repr(C)]
pub struct InstructionCallback {
    /// An opaque pointer to arbitrary state that will be passed to the
    /// callback. If the callback requires no state, this can be null.
    pub state: *mut core::ffi::c_void,
    /// A callback that will be called for each instruction in the transaction.
    /// The callback should return `true` to continue processing instructions,
    /// or `false` to break early.
    pub callback:
        unsafe extern "C" fn(state: *mut core::ffi::c_void, instruction: Instruction) -> bool,
}

/// Iterate over the instructions in the transaction calling the provided
/// callback for each instruction until the callback returns `false` or there
/// are no more instructions.
/// # Safety
/// - The transaction pointer must be valid.
/// - If the callback expects a state, the state must be valid.
/// - The callback must be a valid function pointer.
pub type TransactionIterInstructionsFn = unsafe extern "C" fn(
    transaction_ptr: TransactionPtr,
    instruction_callback: InstructionCallback,
);

/// Return the number of accounts in the transaction.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionNumAccountsFn = unsafe extern "C" fn(transaction_ptr: TransactionPtr) -> usize;

/// Get the account key at the specified index.
/// The returned pointer will be null if the index is out of bounds.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionGetAccountFn =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr, index: usize) -> AccountKey;

/// Returns `true` if the account at index is writable.
/// If the index is out of bounds, this function will return `false`.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionIsWritableFn =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr, index: usize) -> bool;

/// Returns `true` if the account at index is a signer.
/// If the index is out of bounds, this function will return `false`.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionIsSignerFn =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr, index: usize) -> bool;

/// Returns `true` if the account at the specified index is invoked as a
/// program in top-level instructions of this transaction.
/// If the index is out of bounds, this function will return `false`.
/// # Safety
/// - The transaction pointer must be valid.
pub type TransactionIsInvokedFn =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr, index: usize) -> bool;

/// A C-compatible interface that can be used to interact with account keys in
/// transactions. This callback interface is used to inspect account keys in a
/// loop and optionally break early.
#[repr(C)]
pub struct AccountCallback {
    /// An opaque pointer to arbitrary state that will be passed to the
    /// callback. If the callback requires no state, this can be null.
    pub state: *mut core::ffi::c_void,
    /// A callback that will be called for each account key in the transaction.
    /// The callback should return `true` to continue processing account keys,
    /// or `false` to break early.
    pub callback:
        unsafe extern "C" fn(state: *mut core::ffi::c_void, account_key: AccountKey) -> bool,
}

/// Iterate over the account keys in the transaction calling the provided
/// callback for each account key until the callback returns `false` or there
/// are no more account keys.
/// # Safety
/// - The transaction pointer must be valid.
/// - If the callback expects a state, the state must be valid.
/// - The callback must be a valid function pointer.
pub type TransactionIterAccountsFn =
    unsafe extern "C" fn(transaction_ptr: TransactionPtr, account_callback: AccountCallback);

/// A C-compatible interface that can be used to interact with transactions in
/// agave plugins. This interface is used to inspect transactions, not to
/// modify or create them.
/// The actual transaction type is opaque to the plugin and this interface.
#[repr(C)]
pub struct TransactionInterface {
    /// A pointer to the transaction.
    pub transaction_ptr: TransactionPtr,
    /// Returns the number of signatures in this transaction.
    /// See [`TransactionNumSignaturesFn`].
    pub num_signatures_fn: TransactionNumSignaturesFn,
    /// Returns pointer to the first signature in the transaction.
    /// See [`TransactionSignaturesFn`].
    pub signatures_fn: TransactionSignaturesFn,
    /// Returns the total number of signatures in the transaction, including
    /// any pre-compile signatures.
    /// See [`TransactionNumTotalSignatures`].
    pub num_total_signatures_fn: TransactionNumTotalSignatures,
    /// Returns the number of requested write-locks in this transaction.
    /// See [`TransactionNumWriteLocksFn`].
    pub num_write_locks_fn: TransactionNumWriteLocksFn,
    /// Returns a reference to the transaction's recent blockhash.
    /// See [`TransactionRecentBlockhashFn`].
    pub recent_blockhash_fn: TransactionRecentBlockhashFn,
    /// Return the number of instructions in the transaction.
    /// See [`TransactionNumInstructionsFn`].
    pub num_instructions_fn: TransactionNumInstructionsFn,
    /// Iterate over the instructions in the transaction calling the provided
    /// callback for each instruction until the callback returns `false` or there
    /// are no more instructions.
    /// See [`TransactionIterInstructionsFn`].
    pub iter_instructions_fn: TransactionIterInstructionsFn,
    /// Return the number of accounts in the transaction.
    /// See [`TransactionNumAccountsFn`].
    pub num_accounts_fn: TransactionNumAccountsFn,
    /// Get the account key at the specified index.
    /// See [`TransactionGetAccountFn`].
    pub get_account_fn: TransactionGetAccountFn,
    /// Returns `true` if the account at index is writable.
    /// See [`TransactionIsWritableFn`].
    pub is_writable_fn: TransactionIsWritableFn,
    /// Returns `true` if the account at index is a signer.
    /// See [`TransactionIsSignerFn`].
    pub is_signer_fn: TransactionIsSignerFn,
    /// Returns `true` if the account at the specified index is invoked as a
    /// program in top-level instructions of this transaction.
    /// See [`TransactionIsInvokedFn`].
    pub is_invoked_fn: TransactionIsInvokedFn,
    /// Iterate over the account keys in the transaction calling the provided
    /// callback for each account key until the callback returns `false` or there
    /// are no more account keys.
    /// See [`TransactionIterAccountsFn`].
    pub iter_accounts_fn: TransactionIterAccountsFn,
}

// Rust functions to call functions on the `TransactionInterface` struct.
// To avoid comments on unsafe code in each function this top-level comment
// should suffice:
//
// - SAFETY: `TransactionInterface` provided to the plugin has valid
//           transaction pointer and fn pointers.
//           Unless user modified it - these functions are safe.
impl TransactionInterface {
    pub fn num_signatures(&self) -> usize {
        unsafe { (self.num_signatures_fn)(self.transaction_ptr) }
    }

    pub fn signatures(&self) -> &[[u8; 64]] {
        let num_signatures = self.num_signatures();
        unsafe {
            let signatures_ptr = (self.signatures_fn)(self.transaction_ptr);
            core::slice::from_raw_parts(signatures_ptr as *const [u8; 64], num_signatures)
        }
    }

    pub fn num_total_signatures(&self) -> u64 {
        unsafe { (self.num_total_signatures_fn)(self.transaction_ptr) }
    }

    pub fn num_write_locks(&self) -> u64 {
        unsafe { (self.num_write_locks_fn)(self.transaction_ptr) }
    }

    pub fn recent_blockhash(&self) -> &[u8; 32] {
        unsafe {
            let recent_blockhas_ptr = (self.recent_blockhash_fn)(self.transaction_ptr);
            &*(recent_blockhas_ptr as *const [u8; 32])
        }
    }

    pub fn num_instructions(&self) -> usize {
        unsafe { (self.num_instructions_fn)(self.transaction_ptr) }
    }

    pub fn instructions_iter(&self, callback: InstructionCallback) {
        unsafe { (self.iter_instructions_fn)(self.transaction_ptr, callback) }
    }

    pub fn num_accounts(&self) -> usize {
        unsafe { (self.num_accounts_fn)(self.transaction_ptr) }
    }

    pub fn get_account(&self, index: usize) -> Option<&[u8; 32]> {
        unsafe {
            let account_key = (self.get_account_fn)(self.transaction_ptr, index);
            if account_key.is_null() {
                None
            } else {
                Some(&*(account_key as *const [u8; 32]))
            }
        }
    }

    pub fn is_writable(&self, index: usize) -> bool {
        unsafe { (self.is_writable_fn)(self.transaction_ptr, index) }
    }

    pub fn is_signer(&self, index: usize) -> bool {
        unsafe { (self.is_signer_fn)(self.transaction_ptr, index) }
    }

    pub fn is_invoked(&self, index: usize) -> bool {
        unsafe { (self.is_invoked_fn)(self.transaction_ptr, index) }
    }

    pub fn iter_accounts(&self, callback: AccountCallback) {
        unsafe { (self.iter_accounts_fn)(self.transaction_ptr, callback) }
    }
}
