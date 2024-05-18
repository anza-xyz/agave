//! `Instruction`, with a stable memory layout

use {
    crate::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
        stable_layout::stable_vec::StableVec,
    },
    std::{fmt::Debug, marker::PhantomData, mem::ManuallyDrop, ptr::NonNull},
};

/// `Instruction`, with a stable memory layout
///
/// This is used within the runtime to ensure memory mapping and memory accesses are valid.  We
/// rely on known addresses and offsets within the runtime, and since `Instruction`'s layout is
/// allowed to change, we must provide a way to lock down the memory layout.  `StableInstruction`
/// reimplements the bare minimum of `Instruction`'s API sufficient only for the runtime's needs.
///
/// # Examples
///
/// Creating a `StableInstruction` from an `Instruction`
///
/// ```
/// # use solana_program::{instruction::Instruction, pubkey::Pubkey, stable_layout::stable_instruction::StableInstruction};
/// # let program_id = Pubkey::default();
/// # let accounts = Vec::default();
/// # let data = Vec::default();
/// let instruction = Instruction { program_id, accounts, data };
/// let instruction = StableInstruction::from(instruction);
/// ```
#[derive(Debug, PartialEq)]
#[repr(C)]
pub struct StableInstruction {
    pub accounts: StableVec<AccountMeta>,
    pub data: StableVec<u8>,
    pub program_id: Pubkey,
}

impl From<Instruction> for StableInstruction {
    fn from(other: Instruction) -> Self {
        Self {
            accounts: other.accounts.into(),
            data: other.data.into(),
            program_id: other.program_id,
        }
    }
}

/// This wrapper type with no constructor ensures that no user can
/// manually drop the inner type.
///
/// We provide only an immutable borrow method, which ensures that
/// the inner type is not modified in the absence of unsafe code.
///
/// StableInstruction uses NonNull<T> which is invariant over T.
/// NonNull<T> is clonable. It's the same type used by Rc<T> and
/// Arc<T>. It is safe to have an aliasing p
/// ointer to the same
/// allocation as the underlying vectors so long as we perform
/// no modificiations.
///
/// A constructor api is chosen internally over simply making the inner type
/// pub(super) or pub(super) so that not even users within this crate (outside
/// of this module) can modify the inner type.
///
/// Most importantly...
/// No unsafe code was written in the creation or usage of this type :)
pub struct InstructionStabilizer<'a> {
    /// A stable instruction that will not be dropped. By circumventing the
    /// `Drop` implementation, this becomes a view (similar to a slice)
    /// into the original vector's buffer. Since we provide only a borrow
    /// method on this wrapper, we can guarantee that the `StableInstruction`
    /// is never modified.
    stabilized_instruction: core::mem::ManuallyDrop<StableInstruction>,

    /// A read-only view (into the buffers owned by the inner vectors) is
    /// only safe for as long as the `&'a Instruction` lives.
    ///
    /// This could be a `&'a Instruction` but we don't actually need the
    /// instruction. We can pretend to hold a `&'a Instruction`` instead.
    ///
    /// Using a `PhantomData<&'a Instruction>` forces this struct and the
    /// compiler to act like it is holding the reference without increasing
    /// the size of the type.
    phantom_instruction: PhantomData<&'a Instruction>,
}

impl<'ix> InstructionStabilizer<'ix> {
    #[inline(always)]
    pub fn stabilize(instruction: &Instruction) -> InstructionStabilizer {
        stabilize_instruction(instruction)
    }

    /// NOTE:
    ///
    /// A constructor api is chosen internally over simply making the inner type
    /// pub(super) or pub(super) so that not even users within this crate (outside
    /// of this module) can modify the inner type.
    #[inline(always)]
    pub(super) fn new(
        stabilized_instruction: core::mem::ManuallyDrop<StableInstruction>,
        // Note: This is where 'ix is inherited
        _instruction: &'ix Instruction,
    ) -> InstructionStabilizer<'ix> {
        Self {
            stabilized_instruction,
            phantom_instruction: PhantomData::<&'ix Instruction>,
        }
    }

    #[inline(always)]
    pub fn stable_instruction_ref<'borrow>(&'borrow self) -> &'borrow StableInstruction
    where
        // 'ix must live at least as long as 'borrow
        'ix: 'borrow,
    {
        &self.stabilized_instruction
    }

    #[inline(always)]
    pub fn instruction_addr(&self) -> *const u8 {
        self.stable_instruction_ref() as *const StableInstruction as *const u8
    }
}

// Only to be used by super::stable_instruction, but only ancestors are allowed for visibility
#[inline(always)] // only one call site (wrapper fn) so inline there
fn stabilize_instruction<'ix_ref>(ix: &'ix_ref Instruction) -> InstructionStabilizer<'ix_ref> {
    // Get StableVec out of instruction data Vec<u8>
    let data: StableVec<u8> = {
        // Get vector parts
        let ptr = NonNull::new(ix.data.as_ptr() as *mut u8).expect("vector ptr should be valid");
        let len = ix.data.len();
        let cap = ix.data.capacity();

        StableVec {
            ptr,
            cap,
            len,
            _marker: std::marker::PhantomData,
        }
    };

    // Get StableVec out of instruction accounts Vec<Accountmeta>
    let accounts: StableVec<AccountMeta> = {
        // Get vector parts
        let ptr = NonNull::new(ix.accounts.as_ptr() as *mut AccountMeta)
            .expect("vector ptr should be valid");
        let len = ix.accounts.len();
        let cap = ix.accounts.capacity();

        StableVec {
            ptr,
            cap,
            len,
            _marker: std::marker::PhantomData,
        }
    };

    InstructionStabilizer::<'ix_ref>::new(
        ManuallyDrop::new(StableInstruction {
            // Transmuting between identically declared repr(C) structs
            accounts: unsafe { core::mem::transmute(accounts) },
            data: unsafe { core::mem::transmute(data) },
            program_id: ix.program_id,
        }),
        ix,
    )
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        memoffset::offset_of,
        std::mem::{align_of, size_of},
    };

    #[test]
    fn test_memory_layout() {
        assert_eq!(offset_of!(StableInstruction, accounts), 0);
        assert_eq!(offset_of!(StableInstruction, data), 24);
        assert_eq!(offset_of!(StableInstruction, program_id), 48);
        assert_eq!(align_of::<StableInstruction>(), 8);
        assert_eq!(size_of::<StableInstruction>(), 24 + 24 + 32);

        let program_id = Pubkey::new_unique();
        let account_meta1 = AccountMeta {
            pubkey: Pubkey::new_unique(),
            is_signer: true,
            is_writable: false,
        };
        let account_meta2 = AccountMeta {
            pubkey: Pubkey::new_unique(),
            is_signer: false,
            is_writable: true,
        };
        let accounts = vec![account_meta1, account_meta2];
        let data = vec![1, 2, 3, 4, 5];
        let instruction = Instruction {
            program_id,
            accounts: accounts.clone(),
            data: data.clone(),
        };
        let instruction = StableInstruction::from(instruction);

        let instruction_addr = &instruction as *const _ as u64;

        let accounts_ptr = instruction_addr as *const StableVec<AccountMeta>;
        assert_eq!(unsafe { &*accounts_ptr }, &accounts);

        let data_ptr = (instruction_addr + 24) as *const StableVec<u8>;
        assert_eq!(unsafe { &*data_ptr }, &data);

        let pubkey_ptr = (instruction_addr + 48) as *const Pubkey;
        assert_eq!(unsafe { *pubkey_ptr }, program_id);
    }

    #[test]
    fn instruction_stabilizer() {
        // Initialize some instruction to be stabilized
        let instruction = Instruction {
            program_id: Default::default(),
            accounts: Default::default(),
            data: Default::default(),
        };

        // Some context (such as invoke_signed_unchecked where)
        // a &StableInstruction is needed

        let stabilizer = InstructionStabilizer::stabilize(&instruction);
        // This call to drop correctly doesn't compile, due to the
        // `PhantomData<&'a Instruction>` held by the stabilizer.
        // See `instruction_stabilizer_borrow_scope`
        //
        // drop(instruction);
        let stable: &StableInstruction = stabilizer.stable_instruction_ref();
        assert_eq!(&instruction.program_id, &stable.program_id);
        assert_eq!(&instruction.accounts, &stable.accounts);
        assert_eq!(&instruction.data, &stable.data);

        // The invoke syscall actually requires the memory address.
        let instruction_addr: *const u8 = stabilizer.instruction_addr();
        assert_eq!(instruction_addr, stable as *const _ as *const u8);
    }

    #[test]
    fn instruction_stabilizer_borrow_scope() {
        // We want to make sure that the &StableInstruction produced by the stabilizer
        // is always valid. We should not be able to get a &StableInstruction after
        // dropping the original instruction!
        let code = "
            use solana_program::{
                instruction::Instruction,
                stable_layout::stable_instruction::*,
            };

            fn main() {
                // Initialize some instruction to be stabilized
                let instruction = Instruction {
                    program_id: Default::default(),
                    accounts: Default::default(),
                    data: Default::default(),
                };

                let stabilizer = InstructionStabilizer::stabilize(&instruction);
                // This call to drop correctly doesn't compile, due to the
                // `PhantomData<&'a Instruction>` held by the stabilizer.
                // See `instruction_stabilizer_borrow_scope`
                //
                drop(instruction);
                // Invalid instruction borrow, as we've dropped instruction!
                let _stable: &StableInstruction = stabilizer.stable_instruction_ref();
            }
        ";
        trybuild2::TestCases::new().compile_fail_inline(
            "instruction_stabilizer_borrow_scope",
            code,
            "src/stable_layout/instruction_stabilizer_borrow_scope.stderr",
        );
    }
}
