#[deprecated(since = "2.2.0", note = "Use solana-loader-v3-interface instead")]
pub use solana_loader_v3_interface::{
    get_program_data_address,
    instruction::{
        close, close_any, create_buffer, deploy_with_max_program_len, extend_program,
        is_close_instruction, is_set_authority_checked_instruction, is_set_authority_instruction,
        is_upgrade_instruction, set_buffer_authority, set_buffer_authority_checked,
        set_upgrade_authority, set_upgrade_authority_checked, upgrade, write,
    },
    state::UpgradeableLoaderState,
};
pub use solana_sdk_ids::bpf_loader_upgradeable::{check_id, id, ID};
