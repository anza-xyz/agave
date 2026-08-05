use {
    super::*,
    crate::{
        accounts_db::accounts_db_config::{ACCOUNTS_DB_CONFIG_FOR_TESTING, AccountsDbConfig},
        accounts_file::AccountsFileProvider,
    },
};

const DEFAULT_ACCOUNTS_DB_CONFIG: AccountsDbConfig = {
    let mut config = ACCOUNTS_DB_CONFIG_FOR_TESTING;
    config.accounts_file_provider = AccountsFileProvider::AppendVec;
    config
};

#[path = "impl.rs"]
pub(super) mod r#impl;
