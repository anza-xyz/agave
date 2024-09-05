// static account keys has max
use {
    agave_transaction_view::static_account_keys_frame::MAX_STATIC_ACCOUNTS_PER_PACKET as FILTER_SIZE,
    solana_sdk::{message::TransactionSignatureDetails, pubkey::Pubkey},
    solana_svm_transaction::instruction::SVMInstruction,
};

/// Get transaction signature details.
pub fn get_signature_details<'a>(
    num_transaction_signatures: u64,
    instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
) -> TransactionSignatureDetails {
    let mut filter = SignatureDetailsFilter::new();

    // Wrapping arithmetic is safe below because the maximum number of signatures
    // per instruction is 255, and the maximum number of instructions per transaction
    // is low enough that the sum of all signatures will not overflow a u64.
    let mut num_secp256k1_instruction_signatures: u64 = 0;
    let mut num_ed25519_instruction_signatures: u64 = 0;
    for (program_id, instruction) in instructions {
        let program_id_index = instruction.program_id_index;
        match filter.is_signature(program_id_index, program_id) {
            ProgramIdStatus::NotSignature => {}
            ProgramIdStatus::Secp256k1 => {
                num_secp256k1_instruction_signatures = num_secp256k1_instruction_signatures
                    .wrapping_add(u64::from(get_num_signatures_in_instruction(&instruction)));
            }
            ProgramIdStatus::Ed25519 => {
                num_ed25519_instruction_signatures = num_ed25519_instruction_signatures
                    .wrapping_add(u64::from(get_num_signatures_in_instruction(&instruction)));
            }
        }
    }

    TransactionSignatureDetails::new(
        num_transaction_signatures,
        num_secp256k1_instruction_signatures,
        num_ed25519_instruction_signatures,
    )
}

#[inline]
fn get_num_signatures_in_instruction(instruction: &SVMInstruction) -> u8 {
    instruction.data.first().copied().unwrap_or(0)
}

#[derive(Copy, Clone)]
enum ProgramIdStatus {
    NotSignature,
    Secp256k1,
    Ed25519,
}

struct SignatureDetailsFilter {
    // array of slots for all possible static and sanitized program_id_index,
    // each slot indicates if a program_id_index has not been checked, or is
    // already checked with result that can be reused.
    flags: [Option<ProgramIdStatus>; FILTER_SIZE as usize],
}

impl SignatureDetailsFilter {
    #[inline]
    fn new() -> Self {
        Self {
            flags: [None; FILTER_SIZE as usize],
        }
    }

    #[inline]
    fn is_signature(&mut self, index: u8, program_id: &Pubkey) -> ProgramIdStatus {
        let flag = &mut self.flags[usize::from(index)];
        match flag {
            Some(status) => *status,
            None => {
                *flag = Some(Self::check_program_id(program_id));
                *flag.as_ref().unwrap()
            }
        }
    }

    #[inline]
    fn check_program_id(program_id: &Pubkey) -> ProgramIdStatus {
        if program_id == &solana_sdk::secp256k1_program::ID {
            ProgramIdStatus::Secp256k1
        } else if program_id == &solana_sdk::ed25519_program::ID {
            ProgramIdStatus::Ed25519
        } else {
            ProgramIdStatus::NotSignature
        }
    }
}
