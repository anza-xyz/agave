use {
    solana_account_info::AccountInfo, solana_program_error::ProgramError,
    solana_sysvar_id::SysvarId, wincode::DeserializeOwned,
};

pub fn from_account_info<T: DeserializeOwned<Dst = T> + SysvarId>(
    account_info: &AccountInfo,
) -> Result<T, ProgramError> {
    if !T::check_id(account_info.key) {
        return Err(ProgramError::InvalidArgument);
    }
    wincode::deserialize(&account_info.data.borrow()).map_err(|_| ProgramError::InvalidArgument)
}
