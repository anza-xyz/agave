#![allow(clippy::arithmetic_side_effects)]
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate serde_derive;

pub mod parse_account_data;
pub mod parse_address_lookup_table;
pub mod parse_bpf_loader;
#[allow(deprecated)]
pub mod parse_config;
pub mod parse_nonce;
pub mod parse_stake;
pub mod parse_sysvar;
pub mod parse_token;
pub mod parse_token_extension;
pub mod parse_vote;
pub mod validator_info;

pub use solana_ui_account::{
    ParsedAccount, StringAmount, StringDecimals, UiAccount, UiAccountData, UiAccountEncoding,
    UiDataSliceConfig, UiFeeCalculator, MAX_BASE58_BYTES,
};
use {
    crate::parse_account_data::{parse_account_data, AccountAdditionalData},
    base64::{prelude::BASE64_STANDARD, Engine},
    solana_sdk::{account::ReadableAccount, pubkey::Pubkey},
    solana_ui_account::slice_data,
    std::io::Write,
};

fn encode_bs58<T: ReadableAccount>(
    account: &T,
    data_slice_config: Option<UiDataSliceConfig>,
) -> String {
    if account.data().len() <= MAX_BASE58_BYTES {
        bs58::encode(slice_data(account.data(), data_slice_config)).into_string()
    } else {
        "error: data too large for bs58 encoding".to_string()
    }
}

pub fn encode_ui_account<T: ReadableAccount>(
    pubkey: &Pubkey,
    account: &T,
    encoding: UiAccountEncoding,
    additional_data: Option<AccountAdditionalData>,
    data_slice_config: Option<UiDataSliceConfig>,
) -> UiAccount {
    let space = account.data().len();
    let data = match encoding {
        UiAccountEncoding::Binary => {
            let data = encode_bs58(account, data_slice_config);
            UiAccountData::LegacyBinary(data)
        }
        UiAccountEncoding::Base58 => {
            let data = encode_bs58(account, data_slice_config);
            UiAccountData::Binary(data, encoding)
        }
        UiAccountEncoding::Base64 => UiAccountData::Binary(
            BASE64_STANDARD.encode(slice_data(account.data(), data_slice_config)),
            encoding,
        ),
        UiAccountEncoding::Base64Zstd => {
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
            match encoder
                .write_all(slice_data(account.data(), data_slice_config))
                .and_then(|()| encoder.finish())
            {
                Ok(zstd_data) => UiAccountData::Binary(BASE64_STANDARD.encode(zstd_data), encoding),
                Err(_) => UiAccountData::Binary(
                    BASE64_STANDARD.encode(slice_data(account.data(), data_slice_config)),
                    UiAccountEncoding::Base64,
                ),
            }
        }
        UiAccountEncoding::JsonParsed => {
            if let Ok(parsed_data) =
                parse_account_data(pubkey, account.owner(), account.data(), additional_data)
            {
                UiAccountData::Json(parsed_data)
            } else {
                UiAccountData::Binary(
                    BASE64_STANDARD.encode(slice_data(account.data(), data_slice_config)),
                    UiAccountEncoding::Base64,
                )
            }
        }
    };
    UiAccount {
        lamports: account.lamports(),
        data,
        owner: account.owner().to_string(),
        executable: account.executable(),
        rent_epoch: account.rent_epoch(),
        space: Some(space as u64),
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        assert_matches::assert_matches,
        solana_sdk::account::{Account, AccountSharedData},
    };

    #[test]
    fn test_base64_zstd() {
        let encoded_account = encode_ui_account(
            &Pubkey::default(),
            &AccountSharedData::from(Account {
                data: vec![0; 1024],
                ..Account::default()
            }),
            UiAccountEncoding::Base64Zstd,
            None,
            None,
        );
        assert_matches!(
            encoded_account.data,
            UiAccountData::Binary(_, UiAccountEncoding::Base64Zstd)
        );

        let decoded_account = encoded_account.decode::<Account>().unwrap();
        assert_eq!(decoded_account.data(), &vec![0; 1024]);
        let decoded_account = encoded_account.decode::<AccountSharedData>().unwrap();
        assert_eq!(decoded_account.data(), &vec![0; 1024]);
    }
}
