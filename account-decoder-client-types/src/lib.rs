#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
//! Core RPC client types for solana-account-decoder
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#[cfg(feature = "zstd")]
use std::io::Read;
use {
    base64::{prelude::BASE64_STANDARD, Engine},
    core::str::FromStr,
    serde::{Deserialize, Serialize},
    serde_json::Value,
    solana_account::{Account, AccountSharedData},
    solana_pubkey::Pubkey,
    std::sync::Arc,
};
pub mod token;

/// A duplicate representation of an Account for pretty JSON serialization
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UiAccount {
    pub lamports: u64,
    pub data: UiAccountData,
    pub owner: String,
    pub executable: bool,
    pub rent_epoch: u64,
    pub space: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum UiAccountData {
    LegacyBinary(String), // Legacy. Retained for RPC backwards compatibility
    Json(ParsedAccount),
    Binary(String, UiAccountEncoding),
}

impl UiAccountData {
    /// Returns decoded account data in binary format if possible
    pub fn decode(&self) -> Option<Vec<u8>> {
        match self {
            UiAccountData::Json(_) => None,
            UiAccountData::LegacyBinary(blob) => bs58::decode(blob).into_vec().ok(),
            UiAccountData::Binary(blob, encoding) => match encoding {
                UiAccountEncoding::Base58 => bs58::decode(blob).into_vec().ok(),
                UiAccountEncoding::Base64 => BASE64_STANDARD.decode(blob).ok(),
                #[cfg(feature = "zstd")]
                UiAccountEncoding::Base64Zstd => {
                    BASE64_STANDARD.decode(blob).ok().and_then(|zstd_data| {
                        let mut data = vec![];
                        zstd::stream::read::Decoder::new(zstd_data.as_slice())
                            .and_then(|mut reader| reader.read_to_end(&mut data))
                            .map(|_| data)
                            .ok()
                    })
                }
                #[cfg(not(feature = "zstd"))]
                UiAccountEncoding::Base64Zstd => None,
                UiAccountEncoding::Binary | UiAccountEncoding::JsonParsed => None,
            },
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum UiAccountEncoding {
    Binary, // Legacy. Retained for RPC backwards compatibility
    Base58,
    Base64,
    JsonParsed,
    #[serde(rename = "base64+zstd")]
    Base64Zstd,
}

impl UiAccount {
    pub fn to_account_shared_data(&self) -> Option<AccountSharedData> {
        let data = Arc::new(self.data.decode()?);
        Some(AccountSharedData::create_from_existing_shared_data(
            self.lamports,
            data,
            Pubkey::from_str(&self.owner).ok()?,
            self.executable,
            self.rent_epoch,
        ))
    }

    pub fn to_account(&self) -> Option<Account> {
        let data = self.data.decode()?;
        Some(Account {
            lamports: self.lamports,
            data,
            owner: Pubkey::from_str(&self.owner).ok()?,
            executable: self.executable,
            rent_epoch: self.rent_epoch,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedAccount {
    pub program: String,
    pub parsed: Value,
    pub space: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiDataSliceConfig {
    pub offset: usize,
    pub length: usize,
}
