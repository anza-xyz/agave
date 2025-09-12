//! Core RPC client types for solana-account-decoder
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#[cfg(feature = "zstd")]
use std::io::Read;
use {
    base64::{prelude::BASE64_STANDARD, Engine},
    core::str::FromStr,
    serde_derive::{Deserialize, Serialize},
    serde_json::Value,
    solana_account::WritableAccount,
    solana_pubkey::Pubkey,
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
    ///
    /// For `UiAccountData::Json(_)` (JsonParsed), this will return `None` since
    /// the account data has been parsed into a structured format and cannot be
    /// converted back to raw binary data.
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

    /// Returns the account data size from the parsed information if available
    ///
    /// This can extract the size even from JsonParsed accounts.
    pub fn space(&self) -> Option<u64> {
        match self {
            UiAccountData::Json(parsed) => Some(parsed.space),
            UiAccountData::LegacyBinary(blob) => {
                bs58::decode(blob).into_vec().ok().map(|v| v.len() as u64)
            }
            UiAccountData::Binary(blob, encoding) => match encoding {
                UiAccountEncoding::Base58 => {
                    bs58::decode(blob).into_vec().ok().map(|v| v.len() as u64)
                }
                UiAccountEncoding::Base64 => {
                    BASE64_STANDARD.decode(blob).ok().map(|v| v.len() as u64)
                }
                #[cfg(feature = "zstd")]
                UiAccountEncoding::Base64Zstd => {
                    BASE64_STANDARD.decode(blob).ok().and_then(|zstd_data| {
                        let mut data = vec![];
                        zstd::stream::read::Decoder::new(zstd_data.as_slice())
                            .and_then(|mut reader| reader.read_to_end(&mut data))
                            .map(|_| data.len() as u64)
                            .ok()
                    })
                }
                #[cfg(not(feature = "zstd"))]
                UiAccountEncoding::Base64Zstd => None,
                UiAccountEncoding::Binary | UiAccountEncoding::JsonParsed => None,
            },
        }
    }

    /// Returns true if this account data is in JsonParsed format
    pub fn is_json_parsed(&self) -> bool {
        matches!(self, UiAccountData::Json(_))
    }

    /// Returns the parsed account data if this is a JsonParsed account
    pub fn as_parsed(&self) -> Option<&ParsedAccount> {
        match self {
            UiAccountData::Json(parsed) => Some(parsed),
            _ => None,
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
    /// Decode the UiAccount into a concrete Account type
    ///
    /// **Note**: This method will return `None` for accounts with `JsonParsed` encoding
    /// since the account data has been parsed into a structured format and cannot be
    /// converted back to raw binary data. For JsonParsed accounts, use `try_decode_with_fallback()`
    /// or access the parsed data directly via `data.as_parsed()`.
    pub fn decode<T: WritableAccount>(&self) -> Option<T> {
        let data = self.data.decode()?;
        Some(T::create(
            self.lamports,
            data,
            Pubkey::from_str(&self.owner).ok()?,
            self.executable,
            self.rent_epoch,
        ))
    }

    /// Attempts to decode the UiAccount, with fallback handling for JsonParsed accounts
    ///
    /// For JsonParsed accounts where the original binary data is not available,
    /// this method will create an Account with empty data but preserve other metadata.
    /// This is useful for APIs that need to return Account objects but may receive
    /// JsonParsed data.
    ///
    /// Returns a tuple of (Account, is_json_parsed) where the boolean indicates
    /// whether the account data was JsonParsed (and thus the data field is empty).
    pub fn try_decode_with_fallback<T: WritableAccount>(&self) -> Option<(T, bool)> {
        let owner = Pubkey::from_str(&self.owner).ok()?;

        if let Some(data) = self.data.decode() {
            // Normal case - we have binary data
            Some((
                T::create(self.lamports, data, owner, self.executable, self.rent_epoch),
                false,
            ))
        } else if self.data.is_json_parsed() {
            // JsonParsed case - create account with empty data but preserve metadata
            Some((
                T::create(
                    self.lamports,
                    vec![],
                    owner,
                    self.executable,
                    self.rent_epoch,
                ),
                true,
            ))
        } else {
            // Failed to decode for other reasons
            None
        }
    }

    /// Returns true if this account uses JsonParsed encoding
    pub fn is_json_parsed(&self) -> bool {
        self.data.is_json_parsed()
    }

    /// Gets the parsed account data if available
    pub fn parsed_data(&self) -> Option<&ParsedAccount> {
        self.data.as_parsed()
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

#[cfg(test)]
mod tests {
    use {super::*, solana_account::Account};

    #[test]
    fn test_jsonparsed_account_decoding_fix() {
        // Create a sample JsonParsed account (like a token account)
        let parsed_account = ParsedAccount {
            program: "spl-token".to_string(),
            parsed: serde_json::json!({
                "info": {
                    "mint": "So11111111111111111111111111111111111111112",
                    "owner": "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
                    "tokenAmount": {
                        "amount": "1000000000",
                        "decimals": 9,
                        "uiAmount": 1.0
                    }
                },
                "type": "account"
            }),
            space: 165,
        };

        let ui_account = UiAccount {
            lamports: 2039280,
            data: UiAccountData::Json(parsed_account.clone()),
            owner: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            executable: false,
            rent_epoch: 361,
            space: Some(165),
        };

        // Test 1: Original decode() returns None for JsonParsed (this is the original issue)
        let decoded_account: Option<Account> = ui_account.decode();
        assert!(
            decoded_account.is_none(),
            "decode() should return None for JsonParsed accounts"
        );

        // Test 2: New helper methods work
        assert!(
            ui_account.is_json_parsed(),
            "should detect JsonParsed accounts"
        );
        assert_eq!(
            ui_account.data.space(),
            Some(165),
            "should extract space from parsed data"
        );

        // Test 3: Can access parsed data
        let parsed = ui_account.parsed_data().unwrap();
        assert_eq!(parsed.program, "spl-token");
        assert_eq!(parsed.space, 165);

        // Test 4: New fallback method works
        let (account, is_json_parsed) = ui_account.try_decode_with_fallback::<Account>().unwrap();
        assert!(is_json_parsed, "should indicate this was JsonParsed");
        assert_eq!(account.lamports, 2039280);
        assert_eq!(
            account.owner.to_string(),
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        );
        assert!(
            account.data.is_empty(),
            "data should be empty for JsonParsed accounts"
        );
    }

    #[test]
    fn test_binary_account_still_works() {
        let ui_account = UiAccount {
            lamports: 1000000,
            data: UiAccountData::Binary(
                BASE64_STANDARD.encode(b"test data"),
                UiAccountEncoding::Base64,
            ),
            owner: "11111111111111111111111111111111".to_string(),
            executable: false,
            rent_epoch: 361,
            space: Some(9),
        };

        // Original decode() still works for binary accounts
        let decoded_account: Option<Account> = ui_account.decode();
        assert!(
            decoded_account.is_some(),
            "decode() should work for binary accounts"
        );

        let account = decoded_account.unwrap();
        assert_eq!(account.data, b"test data");

        // Fallback method also works
        let (fallback_account, is_json_parsed) =
            ui_account.try_decode_with_fallback::<Account>().unwrap();
        assert!(
            !is_json_parsed,
            "should not indicate JsonParsed for binary accounts"
        );
        assert_eq!(fallback_account.data, b"test data");
    }
}
