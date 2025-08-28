//! Program instructions
use {
    bytemuck::{Pod, Zeroable},
    num_enum::{IntoPrimitive, TryFromPrimitive},
    solana_bls_signatures::Pubkey as BlsPubkey,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
    spl_pod::{bytemuck::pod_bytes_of, primitives::PodU32},
};

/// Instructions supported by the program
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, TryFromPrimitive, IntoPrimitive)]
pub enum VoteInstruction {
    /// Initialize a vote account
    ///
    /// # Account references
    ///   0. `[WRITE]` Uninitialized vote account
    ///   1. `[SIGNER]` New validator identity (node_pubkey)
    ///
    ///   Data expected by this instruction:
    ///     `InitializeAccountInstructionData`
    InitializeAccount,
}

/// Data expected by
/// `VoteInstruction::InitializeAccount`
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct InitializeAccountInstructionData {
    /// The node that votes in this account
    pub node_pubkey: Pubkey,
    /// The signer for vote transactions
    pub authorized_voter: Pubkey,
    /// The signer for withdrawals
    pub authorized_withdrawer: Pubkey,
    /// The commission percentage for this vote account
    pub commission: u8,
    /// BLS public key
    pub bls_pubkey: BlsPubkey,
}

/// Instruction builder to initialize a new vote account with a valid VoteState:
/// - `vote_pubkey` the vote account
/// - `instruction_data` the vote account's account creation metadata
pub fn initialize_account(
    vote_pubkey: Pubkey,
    instruction_data: &InitializeAccountInstructionData,
) -> Instruction {
    let accounts = vec![
        AccountMeta::new(vote_pubkey, false),
        AccountMeta::new_readonly(instruction_data.node_pubkey, true),
    ];

    encode_instruction(
        accounts,
        VoteInstruction::InitializeAccount,
        instruction_data,
    )
}

/// Utility function for encoding instruction data
pub(crate) fn encode_instruction<D: Pod>(
    accounts: Vec<AccountMeta>,
    instruction: VoteInstruction,
    instruction_data: &D,
) -> Instruction {
    encode_instruction_with_seed(accounts, instruction, instruction_data, None)
}

/// Utility function for encoding instruction data
/// with a seed.
///
/// Some accounting instructions have a variable length
/// `seed`, we serialize this as a pod slice at the end
/// of the instruction data
pub(crate) fn encode_instruction_with_seed<D: Pod>(
    accounts: Vec<AccountMeta>,
    instruction: VoteInstruction,
    instruction_data: &D,
    seed: Option<&str>,
) -> Instruction {
    let mut data = vec![u8::from(instruction)];
    data.extend_from_slice(bytemuck::bytes_of(instruction_data));
    if let Some(seed) = seed {
        let seed_len = PodU32::from(seed.len() as u32);
        data.extend_from_slice(&[pod_bytes_of(&seed_len), seed.as_bytes()].concat());
    }
    Instruction {
        program_id: crate::id(),
        accounts,
        data,
    }
}
