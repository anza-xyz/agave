use {
    crate::{transaction_data::TransactionData, transaction_view::TransactionView},
    core::fmt::{Debug, Formatter},
    solana_sdk::{
        bpf_loader_upgradeable, ed25519_program,
        hash::Hash,
        message::{v0::LoadedAddresses, AccountKeys, TransactionSignatureDetails},
        pubkey::Pubkey,
        secp256k1_program,
    },
    solana_svm_transaction::{
        instruction::SVMInstruction, message_address_table_lookup::SVMMessageAddressTableLookup,
        svm_message::SVMMessage,
    },
    std::collections::HashSet,
};

/// A parsed and sanitized transaction view that has had all address lookups
/// resolved.
pub struct ResolvedTransactionView<D: TransactionData> {
    /// The parsed and sanitized transction view.
    view: TransactionView<true, D>,
    /// The resolved address lookups.
    resolved_addresses: LoadedAddresses,
    /// A cache for whether an address is writable.
    writable_cache: Vec<bool>, // TODO: should this be a vec, bitset, or array[256].
}

impl<D: TransactionData> Debug for ResolvedTransactionView<D> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedTransactionView")
            .field("view", &self.view)
            .finish()
    }
}

impl<D: TransactionData> ResolvedTransactionView<D> {
    /// Given a parsed and sanitized transaction view, and a set of resolved
    /// addresses, create a resolved transaction view.
    pub fn new(
        view: TransactionView<true, D>,
        resolved_addresses: LoadedAddresses,
        reserved_account_keys: &HashSet<Pubkey>, // why does this not use ahash at least?!
    ) -> Self {
        let writable_cache =
            Self::cache_is_writable(&view, &resolved_addresses, reserved_account_keys);
        Self {
            view,
            resolved_addresses,
            writable_cache,
        }
    }

    /// Helper function to check if an address is writable,
    /// and cache the result.
    /// This is done so we avoid recomputing the expensive checks each time we call
    /// `is_writable` - since there is more to it than just checking index.
    fn cache_is_writable(
        view: &TransactionView<true, D>,
        resolved_addresses: &LoadedAddresses,
        reserved_account_keys: &HashSet<Pubkey>, // why does this not use ahash at least?!
    ) -> Vec<bool> {
        // Build account keys so that we can iterate over and check if
        // an address is writable.
        let account_keys = AccountKeys::new(view.static_account_keys(), Some(resolved_addresses));

        let mut is_writable_cache = Vec::with_capacity(account_keys.len());
        let num_static_accounts = usize::from(view.num_static_account_keys());
        let num_writable_lookup_accounts = usize::from(view.total_writable_lookup_accounts());
        let num_signed_accounts = usize::from(view.num_required_signatures());
        let num_unsigned_accounts = num_static_accounts.wrapping_sub(num_signed_accounts);
        let num_writable_unsigned_accounts =
            num_unsigned_accounts.wrapping_sub(usize::from(view.num_readonly_unsigned_accounts()));
        let num_writable_signed_accounts =
            num_signed_accounts.wrapping_sub(usize::from(view.num_readonly_signed_accounts()));

        let is_upgradable_loader_present = account_keys
            .iter()
            .any(|key| bpf_loader_upgradeable::ID == *key);

        for (index, key) in account_keys.iter().enumerate() {
            let is_requested_write = {
                // If the account is a resolved address, check if it is writable.
                if index >= num_static_accounts {
                    let loaded_address_index = index.wrapping_sub(num_static_accounts);
                    loaded_address_index < num_writable_lookup_accounts
                } else if index >= num_signed_accounts {
                    let unsigned_account_index = index.wrapping_sub(num_signed_accounts);
                    unsigned_account_index < num_writable_unsigned_accounts
                } else {
                    index < num_writable_signed_accounts
                }
            };

            // If the key is reserved it cannot be writable.
            is_writable_cache[index] = is_requested_write && !reserved_account_keys.contains(key);
        }

        // If the upgradable loader is not present and a key is called as a program
        // it is not writable.
        // If the upgradable loader is present, then there is no point to loop.
        // Looping over the instructions is more efficient than looping over each account
        // and checking `is_invoked` which loops over each instruction internally.
        if !is_upgradable_loader_present {
            for ix in view.instructions_iter() {
                is_writable_cache[usize::from(ix.program_id_index)] = false;
            }
        }

        is_writable_cache
    }

    fn num_readonly_accounts(&self) -> usize {
        usize::from(
            self.view
                .total_readonly_lookup_accounts()
                .wrapping_add(u16::from(self.view.num_readonly_signed_accounts()))
                .wrapping_add(u16::from(self.view.num_readonly_unsigned_accounts())),
        )
    }

    fn signature_details(&self) -> TransactionSignatureDetails {
        // counting the number of pre-processor operations separately
        let mut num_secp256k1_instruction_signatures: u64 = 0;
        let mut num_ed25519_instruction_signatures: u64 = 0;
        for (program_id, instruction) in self.program_instructions_iter() {
            if secp256k1_program::check_id(program_id) {
                if let Some(num_verifies) = instruction.data.first() {
                    num_secp256k1_instruction_signatures =
                        num_secp256k1_instruction_signatures.wrapping_add(u64::from(*num_verifies));
                }
            } else if ed25519_program::check_id(program_id) {
                if let Some(num_verifies) = instruction.data.first() {
                    num_ed25519_instruction_signatures =
                        num_ed25519_instruction_signatures.wrapping_add(u64::from(*num_verifies));
                }
            }
        }

        TransactionSignatureDetails {
            num_transaction_signatures: u64::from(self.view.num_required_signatures()),
            num_secp256k1_instruction_signatures,
            num_ed25519_instruction_signatures,
        }
    }
}

impl<D: TransactionData> SVMMessage for ResolvedTransactionView<D> {
    fn num_total_signatures(&self) -> u64 {
        self.signature_details().total_signatures()
    }

    fn num_write_locks(&self) -> u64 {
        self.account_keys()
            .len()
            .wrapping_sub(self.num_readonly_accounts()) as u64
    }

    fn recent_blockhash(&self) -> &Hash {
        self.view.recent_blockhash()
    }

    fn num_instructions(&self) -> usize {
        usize::from(self.view.num_instructions())
    }

    fn instructions_iter(&self) -> impl Iterator<Item = SVMInstruction> {
        self.view.instructions_iter()
    }

    fn program_instructions_iter(
        &self,
    ) -> impl Iterator<
        Item = (
            &solana_sdk::pubkey::Pubkey,
            solana_svm_transaction::instruction::SVMInstruction,
        ),
    > {
        self.view.program_instructions_iter()
    }

    fn account_keys(&self) -> AccountKeys {
        AccountKeys::new(
            self.view.static_account_keys(),
            Some(&self.resolved_addresses), // TODO: should this always be Some?
        )
    }

    fn fee_payer(&self) -> &Pubkey {
        &self.view.static_account_keys()[0]
    }

    fn is_writable(&self, index: usize) -> bool {
        self.writable_cache[index]
    }

    fn is_signer(&self, index: usize) -> bool {
        index < usize::from(self.view.num_required_signatures())
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        let Ok(index) = u8::try_from(key_index) else {
            return false;
        };
        self.view
            .instructions_iter()
            .any(|ix| ix.program_id_index == index)
    }

    fn num_lookup_tables(&self) -> usize {
        usize::from(self.view.num_address_table_lookups())
    }

    fn message_address_table_lookups(&self) -> impl Iterator<Item = SVMMessageAddressTableLookup> {
        self.view.address_table_lookup_iter()
    }
}
