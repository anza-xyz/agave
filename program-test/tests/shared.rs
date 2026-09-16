use {
    serde::de::DeserializeOwned, solana_account_info::AccountInfo,
    solana_program_error::ProgramError, solana_sysvar_id::SysvarId,
};

pub fn from_account_info<T: DeserializeOwned + SysvarId>(
    account_info: &AccountInfo,
) -> Result<T, ProgramError> {
    if !T::check_id(account_info.key) {
        return Err(ProgramError::InvalidArgument);
    }
    bincode::deserialize(&account_info.data.borrow()).map_err(|_| ProgramError::InvalidArgument)
}
