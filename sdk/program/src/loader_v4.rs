#[deprecated(since = "2.2.0", note = "Use solana-loader-v4-interface instead")]
pub use solana_loader_v4_interface::{
    instruction::{
        create_buffer, deploy, deploy_from_source, finalize, is_deploy_instruction,
        is_finalize_instruction, is_retract_instruction, is_set_program_length_instruction,
        is_set_program_length_instruction as is_truncate_instruction,
        is_transfer_authority_instruction, is_write_instruction, retract,
        set_program_length as truncate, set_program_length as truncate_uninitialized,
        set_program_length, transfer_authority, write,
    },
    state::{LoaderV4State, LoaderV4Status},
    DEPLOYMENT_COOLDOWN_IN_SLOTS,
};
pub use solana_sdk_ids::loader_v4::{check_id, id, ID};
