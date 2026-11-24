//! SIMD-0674 location registration instruction, pending its addition to the SDK.
use {
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    wincode::{SchemaRead, SchemaWrite},
};

#[derive(Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub enum LocationInstruction {
    #[wincode(tag = 20)]
    UpdateLocation { x: i32, y: i32, z: i32 },
}

/// Register integer WGS84 ECEF coordinates, signed by the current vote authority.
pub fn update_location(
    vote_account: &Pubkey,
    authorized_voter: &Pubkey,
    x: i32,
    y: i32,
    z: i32,
) -> Instruction {
    Instruction::new_with_bytes(
        solana_sdk_ids::vote::id(),
        &wincode::serialize(&LocationInstruction::UpdateLocation { x, y, z }).unwrap(),
        vec![
            AccountMeta::new(*vote_account, false),
            AccountMeta::new_readonly(*authorized_voter, true),
        ],
    )
}
