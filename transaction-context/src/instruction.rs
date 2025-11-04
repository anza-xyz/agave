use {
    crate::{
        instruction_accounts::BorrowedInstructionAccount, IndexOfAccount, InstructionAccount,
        TransactionContext,
    },
    solana_account::ReadableAccount,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    std::{borrow::Cow, collections::HashSet},
};

/// Instruction shared between runtime and programs.
#[derive(Debug, Clone, Default)]
pub struct InstructionFrame<'ix_data> {
    pub nesting_level: usize,
    pub program_account_index_in_tx: IndexOfAccount,
    pub instruction_accounts: Vec<InstructionAccount>,
    /// This is an account deduplication map that maps index_in_transaction to index_in_instruction
    /// Usage: dedup_map[index_in_transaction] = index_in_instruction
    /// This is a vector of u8s to save memory, since many entries may be unused.
    pub(crate) dedup_map: Vec<u8>,
    pub instruction_data: Cow<'ix_data, [u8]>,
}

/// View interface to read instructions.
#[derive(Debug, Clone)]
pub struct InstructionContext<'a, 'ix_data> {
    pub(crate) transaction_context: &'a TransactionContext<'ix_data>,
    // The rest of the fields are redundant shortcuts
    pub(crate) index_in_trace: usize,
    pub(crate) nesting_level: usize,
    pub(crate) program_account_index_in_tx: IndexOfAccount,
    pub(crate) instruction_accounts: &'a [InstructionAccount],
    pub(crate) dedup_map: &'a [u8],
    pub(crate) instruction_data: &'ix_data [u8],
}

impl<'a> InstructionContext<'a, '_> {
    /// How many Instructions were on the trace before this one was pushed
    pub fn get_index_in_trace(&self) -> usize {
        self.index_in_trace
    }

    /// How many Instructions were on the stack after this one was pushed
    ///
    /// That is the number of nested parent Instructions plus one (itself).
    pub fn get_stack_height(&self) -> usize {
        self.nesting_level.saturating_add(1)
    }

    /// Number of accounts in this Instruction (without program accounts)
    pub fn get_number_of_instruction_accounts(&self) -> IndexOfAccount {
        self.instruction_accounts.len() as IndexOfAccount
    }

    /// Assert that enough accounts were supplied to this Instruction
    pub fn check_number_of_instruction_accounts(
        &self,
        expected_at_least: IndexOfAccount,
    ) -> Result<(), InstructionError> {
        if self.get_number_of_instruction_accounts() < expected_at_least {
            Err(InstructionError::MissingAccount)
        } else {
            Ok(())
        }
    }

    /// Data parameter for the programs `process_instruction` handler
    pub fn get_instruction_data(&self) -> &[u8] {
        self.instruction_data
    }

    /// Translates the given instruction wide program_account_index into a transaction wide index
    pub fn get_index_of_program_account_in_transaction(
        &self,
    ) -> Result<IndexOfAccount, InstructionError> {
        if self.program_account_index_in_tx == u16::MAX {
            Err(InstructionError::MissingAccount)
        } else {
            Ok(self.program_account_index_in_tx)
        }
    }

    /// Translates the given instruction wide instruction_account_index into a transaction wide index
    pub fn get_index_of_instruction_account_in_transaction(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<IndexOfAccount, InstructionError> {
        Ok(self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .index_in_transaction as IndexOfAccount)
    }

    /// Get the index of account in instruction from the index in transaction
    pub fn get_index_of_account_in_instruction(
        &self,
        index_in_transaction: IndexOfAccount,
    ) -> Result<IndexOfAccount, InstructionError> {
        self.dedup_map
            .get(index_in_transaction as usize)
            .and_then(|idx| {
                if *idx as usize >= self.instruction_accounts.len() {
                    None
                } else {
                    Some(*idx as IndexOfAccount)
                }
            })
            .ok_or(InstructionError::MissingAccount)
    }

    /// Returns `Some(instruction_account_index)` if this is a duplicate
    /// and `None` if it is the first account with this key
    pub fn is_instruction_account_duplicate(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Option<IndexOfAccount>, InstructionError> {
        let index_in_transaction =
            self.get_index_of_instruction_account_in_transaction(instruction_account_index)?;
        let first_instruction_account_index =
            self.get_index_of_account_in_instruction(index_in_transaction)?;

        Ok(
            if first_instruction_account_index == instruction_account_index {
                None
            } else {
                Some(first_instruction_account_index)
            },
        )
    }

    /// Gets the key of the last program account of this Instruction
    pub fn get_program_key(&self) -> Result<&'a Pubkey, InstructionError> {
        self.get_index_of_program_account_in_transaction()
            .and_then(|index_in_transaction| {
                self.transaction_context
                    .get_key_of_account_at_index(index_in_transaction)
            })
    }

    /// Get the owner of the program account of this instruction
    pub fn get_program_owner(&self) -> Result<Pubkey, InstructionError> {
        self.get_index_of_program_account_in_transaction()
            .and_then(|index_in_transaction| {
                self.transaction_context
                    .accounts
                    .try_borrow(index_in_transaction)
            })
            .map(|acc| *acc.owner())
    }

    /// Gets an instruction account of this Instruction
    pub fn try_borrow_instruction_account(
        &self,
        index_in_instruction: IndexOfAccount,
    ) -> Result<BorrowedInstructionAccount<'_, '_>, InstructionError> {
        let instruction_account = *self
            .instruction_accounts
            .get(index_in_instruction as usize)
            .ok_or(InstructionError::MissingAccount)?;

        let account = self
            .transaction_context
            .accounts
            .try_borrow_mut(instruction_account.index_in_transaction)?;

        Ok(BorrowedInstructionAccount {
            transaction_context: self.transaction_context,
            instruction_account,
            account,
            index_in_transaction_of_instruction_program: self.program_account_index_in_tx,
        })
    }

    /// Returns whether an instruction account is a signer
    pub fn is_instruction_account_signer(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<bool, InstructionError> {
        Ok(self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .is_signer())
    }

    /// Returns whether an instruction account is writable
    pub fn is_instruction_account_writable(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<bool, InstructionError> {
        Ok(self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .is_writable())
    }

    /// Calculates the set of all keys of signer instruction accounts in this Instruction
    pub fn get_signers(&self) -> Result<HashSet<Pubkey>, InstructionError> {
        let mut result = HashSet::new();
        for instruction_account in self.instruction_accounts.iter() {
            if instruction_account.is_signer() {
                result.insert(
                    *self
                        .transaction_context
                        .get_key_of_account_at_index(instruction_account.index_in_transaction)?,
                );
            }
        }
        Ok(result)
    }

    pub fn instruction_accounts(&self) -> &[InstructionAccount] {
        self.instruction_accounts
    }

    pub fn get_key_of_instruction_account(
        &self,
        index_in_instruction: IndexOfAccount,
    ) -> Result<&'a Pubkey, InstructionError> {
        self.get_index_of_instruction_account_in_transaction(index_in_instruction)
            .and_then(|idx| self.transaction_context.get_key_of_account_at_index(idx))
    }
}
