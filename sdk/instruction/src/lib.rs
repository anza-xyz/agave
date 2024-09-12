//! Types for directing the execution of Solana programs.
//!
//! Every invocation of a Solana program executes a single instruction, as
//! defined by the [`Instruction`] type. An instruction is primarily a vector of
//! bytes, the contents of which are program-specific, and not interpreted by
//! the Solana runtime. This allows flexibility in how programs behave, how they
//! are controlled by client software, and what data encodings they use.
//!
//! Besides the instruction data, every account a program may read or write
//! while executing a given instruction is also included in `Instruction`, as
//! [`AccountMeta`] values. The runtime uses this information to efficiently
//! schedule execution of transactions.
#![cfg_attr(RUSTC_WITH_SPECIALIZATION, feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]

#[cfg(feature = "borsh")]
use borsh::BorshSerialize;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::wasm_bindgen;
#[cfg(feature = "serde")]
use {
    serde_derive::{Deserialize, Serialize},
    solana_short_vec as short_vec,
};
use {solana_pubkey::Pubkey, solana_sanitize::Sanitize};
pub mod account_meta;
pub use account_meta::AccountMeta;
pub mod error;
#[cfg(target_os = "solana")]
pub mod syscalls;

/// A directive for a single invocation of a Solana program.
///
/// An instruction specifies which program it is calling, which accounts it may
/// read or modify, and additional data that serves as input to the program. One
/// or more instructions are included in transactions submitted by Solana
/// clients. Instructions are also used to describe [cross-program
/// invocations][cpi].
///
/// [cpi]: https://solana.com/docs/core/cpi
///
/// During execution, a program will receive a list of account data as one of
/// its arguments, in the same order as specified during `Instruction`
/// construction.
///
/// While Solana is agnostic to the format of the instruction data, it has
/// built-in support for serialization via [`borsh`] and [`bincode`].
///
/// [`borsh`]: https://docs.rs/borsh/latest/borsh/
/// [`bincode`]: https://docs.rs/bincode/latest/bincode/
///
/// # Specifying account metadata
///
/// When constructing an [`Instruction`], a list of all accounts that may be
/// read or written during the execution of that instruction must be supplied as
/// [`AccountMeta`] values.
///
/// Any account whose data may be mutated by the program during execution must
/// be specified as writable. During execution, writing to an account that was
/// not specified as writable will cause the transaction to fail. Writing to an
/// account that is not owned by the program will cause the transaction to fail.
///
/// Any account whose lamport balance may be mutated by the program during
/// execution must be specified as writable. During execution, mutating the
/// lamports of an account that was not specified as writable will cause the
/// transaction to fail. While _subtracting_ lamports from an account not owned
/// by the program will cause the transaction to fail, _adding_ lamports to any
/// account is allowed, as long is it is mutable.
///
/// Accounts that are not read or written by the program may still be specified
/// in an `Instruction`'s account list. These will affect scheduling of program
/// execution by the runtime, but will otherwise be ignored.
///
/// When building a transaction, the Solana runtime coalesces all accounts used
/// by all instructions in that transaction, along with accounts and permissions
/// required by the runtime, into a single account list. Some accounts and
/// account permissions required by the runtime to process a transaction are
/// _not_ required to be included in an `Instruction`s account list. These
/// include:
///
/// - The program ID &mdash; it is a separate field of `Instruction`
/// - The transaction's fee-paying account &mdash; it is added during [`Message`]
///   construction. A program may still require the fee payer as part of the
///   account list if it directly references it.
///
/// [`Message`]: https://docs.rs/solana-program/latest/solana_program/message/legacy/struct.Message.html
///
/// Programs may require signatures from some accounts, in which case they
/// should be specified as signers during `Instruction` construction. The
/// program must still validate during execution that the account is a signer.
#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Instruction {
    /// Pubkey of the program that executes this instruction.
    pub program_id: Pubkey,
    /// Metadata describing accounts that should be passed to the program.
    pub accounts: Vec<AccountMeta>,
    /// Opaque data passed to the program for its own interpretation.
    pub data: Vec<u8>,
}

/// wasm-bindgen version of the Instruction struct.
/// This duplication is required until https://github.com/rustwasm/wasm-bindgen/issues/3671
/// is fixed. This must not diverge from the regular non-wasm Instruction struct.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub struct Instruction {
    #[wasm_bindgen(skip)]
    pub program_id: Pubkey,
    #[wasm_bindgen(skip)]
    pub accounts: Vec<AccountMeta>,
    #[wasm_bindgen(skip)]
    pub data: Vec<u8>,
}

impl Instruction {
    #[cfg(feature = "borsh")]
    /// Create a new instruction from a value, encoded with [`borsh`].
    ///
    /// [`borsh`]: https://docs.rs/borsh/latest/borsh/
    ///
    /// `program_id` is the address of the program that will execute the instruction.
    /// `accounts` contains a description of all accounts that may be accessed by the program.
    ///
    /// Borsh serialization is often preferred over bincode as it has a stable
    /// [specification] and an [implementation in JavaScript][jsb], neither of
    /// which are true of bincode.
    ///
    /// [specification]: https://borsh.io/
    /// [jsb]: https://github.com/near/borsh-js
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_pubkey::Pubkey;
    /// # use solana_instruction::{AccountMeta, Instruction};
    /// # use borsh::{BorshSerialize, BorshDeserialize};
    /// #
    /// #[derive(BorshSerialize, BorshDeserialize)]
    /// # #[borsh(crate = "borsh")]
    /// pub struct MyInstruction {
    ///     pub lamports: u64,
    /// }
    ///
    /// pub fn create_instruction(
    ///     program_id: &Pubkey,
    ///     from: &Pubkey,
    ///     to: &Pubkey,
    ///     lamports: u64,
    /// ) -> Instruction {
    ///     let instr = MyInstruction { lamports };
    ///
    ///     Instruction::new_with_borsh(
    ///         *program_id,
    ///         &instr,
    ///         vec![
    ///             AccountMeta::new(*from, true),
    ///             AccountMeta::new(*to, false),
    ///         ],
    ///    )
    /// }
    /// ```
    pub fn new_with_borsh<T: BorshSerialize>(
        program_id: Pubkey,
        data: &T,
        accounts: Vec<AccountMeta>,
    ) -> Self {
        let data = borsh::to_vec(data).unwrap();
        Self {
            program_id,
            accounts,
            data,
        }
    }

    #[cfg(feature = "bincode")]
    /// Create a new instruction from a value, encoded with [`bincode`].
    ///
    /// [`bincode`]: https://docs.rs/bincode/latest/bincode/
    ///
    /// `program_id` is the address of the program that will execute the instruction.
    /// `accounts` contains a description of all accounts that may be accessed by the program.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_pubkey::Pubkey;
    /// # use solana_instruction::{AccountMeta, Instruction};
    /// # use serde::{Serialize, Deserialize};
    /// #
    /// #[derive(Serialize, Deserialize)]
    /// pub struct MyInstruction {
    ///     pub lamports: u64,
    /// }
    ///
    /// pub fn create_instruction(
    ///     program_id: &Pubkey,
    ///     from: &Pubkey,
    ///     to: &Pubkey,
    ///     lamports: u64,
    /// ) -> Instruction {
    ///     let instr = MyInstruction { lamports };
    ///
    ///     Instruction::new_with_bincode(
    ///         *program_id,
    ///         &instr,
    ///         vec![
    ///             AccountMeta::new(*from, true),
    ///             AccountMeta::new(*to, false),
    ///         ],
    ///    )
    /// }
    /// ```
    pub fn new_with_bincode<T: serde::Serialize>(
        program_id: Pubkey,
        data: &T,
        accounts: Vec<AccountMeta>,
    ) -> Self {
        let data = bincode::serialize(data).unwrap();
        Self {
            program_id,
            accounts,
            data,
        }
    }

    /// Create a new instruction from a byte slice.
    ///
    /// `program_id` is the address of the program that will execute the instruction.
    /// `accounts` contains a description of all accounts that may be accessed by the program.
    ///
    /// The caller is responsible for ensuring the correct encoding of `data` as expected
    /// by the callee program.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_pubkey::Pubkey;
    /// # use solana_instruction::{AccountMeta, Instruction};
    /// #
    /// # use borsh::{io::Error, BorshSerialize, BorshDeserialize};
    /// #
    /// #[derive(BorshSerialize, BorshDeserialize)]
    /// # #[borsh(crate = "borsh")]
    /// pub struct MyInstruction {
    ///     pub lamports: u64,
    /// }
    ///
    /// pub fn create_instruction(
    ///     program_id: &Pubkey,
    ///     from: &Pubkey,
    ///     to: &Pubkey,
    ///     lamports: u64,
    /// ) -> Result<Instruction, Error> {
    ///     let instr = MyInstruction { lamports };
    ///
    ///     let mut instr_in_bytes: Vec<u8> = Vec::new();
    ///     instr.serialize(&mut instr_in_bytes)?;
    ///
    ///     Ok(Instruction::new_with_bytes(
    ///         *program_id,
    ///         &instr_in_bytes,
    ///         vec![
    ///             AccountMeta::new(*from, true),
    ///             AccountMeta::new(*to, false),
    ///         ],
    ///    ))
    /// }
    /// ```
    pub fn new_with_bytes(program_id: Pubkey, data: &[u8], accounts: Vec<AccountMeta>) -> Self {
        Self {
            program_id,
            accounts,
            data: data.to_vec(),
        }
    }
}

/// A compact encoding of an instruction.
///
/// A `CompiledInstruction` is a component of a multi-instruction [`Message`],
/// which is the core of a Solana transaction. It is created during the
/// construction of `Message`. Most users will not interact with it directly.
///
/// [`Message`]: https://docs.rs/solana-program/latest/solana_program/message/legacy/struct.Message.html
#[cfg_attr(feature = "frozen-abi", derive(solana_frozen_abi_macro::AbiExample))]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "camelCase")
)]
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct CompiledInstruction {
    /// Index into the transaction keys array indicating the program account that executes this instruction.
    pub program_id_index: u8,
    /// Ordered indices into the transaction keys array indicating which accounts to pass to the program.
    #[cfg_attr(feature = "serde", serde(with = "short_vec"))]
    pub accounts: Vec<u8>,
    /// The program input data.
    #[cfg_attr(feature = "serde", serde(with = "short_vec"))]
    pub data: Vec<u8>,
}

impl Sanitize for CompiledInstruction {}

impl CompiledInstruction {
    #[cfg(feature = "bincode")]
    pub fn new<T: serde::Serialize>(program_ids_index: u8, data: &T, accounts: Vec<u8>) -> Self {
        let data = bincode::serialize(data).unwrap();
        Self {
            program_id_index: program_ids_index,
            accounts,
            data,
        }
    }

    pub fn new_from_raw_parts(program_id_index: u8, data: Vec<u8>, accounts: Vec<u8>) -> Self {
        Self {
            program_id_index,
            accounts,
            data,
        }
    }

    pub fn program_id<'a>(&self, program_ids: &'a [Pubkey]) -> &'a Pubkey {
        &program_ids[self.program_id_index as usize]
    }
}

// Stack height when processing transaction-level instructions
pub const TRANSACTION_LEVEL_STACK_HEIGHT: usize = 1;
