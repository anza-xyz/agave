#![allow(deprecated)]
use {
    solana_inline_spl::{token::GenericTokenAccount, token_2022::Account},
    solana_sdk::account::{AccountSharedData, ReadableAccount},
    std::borrow::Cow,
    thiserror::Error,
};

const MAX_DATA_SIZE: usize = 128;
const MAX_DATA_BASE58_SIZE: usize = 175;
const MAX_DATA_BASE64_SIZE: usize = 172;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcFilterType {
    DataSize(u64),
    Memcmp(Memcmp),
    TokenAccountState,
}

impl RpcFilterType {
    pub fn verify(&self) -> Result<(), RpcFilterError> {
        match self {
            RpcFilterType::DataSize(_) => Ok(()),
            RpcFilterType::Memcmp(compare) => {
                let encoding = compare.encoding.as_ref().unwrap_or(&MemcmpEncoding::Binary);
                match encoding {
                    MemcmpEncoding::Binary => {
                        use MemcmpEncodedBytes::*;
                        match &compare.bytes {
                            // DEPRECATED
                            Binary(bytes) => {
                                if bytes.len() > MAX_DATA_BASE58_SIZE {
                                    return Err(RpcFilterError::Base58DataTooLarge);
                                }
                                let bytes = bs58::decode(&bytes)
                                    .into_vec()
                                    .map_err(RpcFilterError::DecodeError)?;
                                if bytes.len() > MAX_DATA_SIZE {
                                    Err(RpcFilterError::Base58DataTooLarge)
                                } else {
                                    Ok(())
                                }
                            }
                            Base58(bytes) => {
                                if bytes.len() > MAX_DATA_BASE58_SIZE {
                                    return Err(RpcFilterError::DataTooLarge);
                                }
                                let bytes = bs58::decode(&bytes).into_vec()?;
                                if bytes.len() > MAX_DATA_SIZE {
                                    Err(RpcFilterError::DataTooLarge)
                                } else {
                                    Ok(())
                                }
                            }
                            Base64(bytes) => {
                                if bytes.len() > MAX_DATA_BASE64_SIZE {
                                    return Err(RpcFilterError::DataTooLarge);
                                }
                                let bytes = base64::decode(bytes)?;
                                if bytes.len() > MAX_DATA_SIZE {
                                    Err(RpcFilterError::DataTooLarge)
                                } else {
                                    Ok(())
                                }
                            }
                            Bytes(bytes) => {
                                if bytes.len() > MAX_DATA_SIZE {
                                    return Err(RpcFilterError::DataTooLarge);
                                }
                                Ok(())
                            }
                        }
                    }
                }
            }
            RpcFilterType::TokenAccountState => Ok(()),
        }
    }

    #[deprecated(
        since = "2.0.0",
        note = "Use solana_rpc::filter::filter_allows instead"
    )]
    pub fn allows(&self, account: &AccountSharedData) -> bool {
        match self {
            RpcFilterType::DataSize(size) => account.data().len() as u64 == *size,
            RpcFilterType::Memcmp(compare) => compare.bytes_match(account.data()),
            RpcFilterType::TokenAccountState => Account::valid_account_data(account.data()),
        }
    }
}

#[derive(Error, PartialEq, Eq, Debug)]
pub enum RpcFilterError {
    #[error("encoded binary data should be less than 129 bytes")]
    DataTooLarge,
    #[deprecated(
        since = "1.8.1",
        note = "Error for MemcmpEncodedBytes::Binary which is deprecated"
    )]
    #[error("encoded binary (base 58) data should be less than 129 bytes")]
    Base58DataTooLarge,
    #[deprecated(
        since = "1.8.1",
        note = "Error for MemcmpEncodedBytes::Binary which is deprecated"
    )]
    #[error("bs58 decode error")]
    DecodeError(bs58::decode::Error),
    #[error("base58 decode error")]
    Base58DecodeError(#[from] bs58::decode::Error),
    #[error("base64 decode error")]
    Base64DecodeError(#[from] base64::DecodeError),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemcmpEncoding {
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "encoding", content = "bytes")]
pub enum MemcmpEncodedBytes {
    #[deprecated(
        since = "1.8.1",
        note = "Please use MemcmpEncodedBytes::Base58 instead"
    )]
    Binary(String),
    Base58(String),
    Base64(String),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Memcmp {
    /// Data offset to begin match
    offset: usize,
    /// Bytes, encoded with specified encoding, or default Binary
    #[serde(flatten)]
    bytes: MemcmpEncodedBytes,
    /// Optional encoding specification
    #[deprecated(
        since = "1.11.2",
        note = "Field has no server-side effect. Specify encoding with `MemcmpEncodedBytes` variant instead. \
            Field will be made private in future. Please use a constructor method instead."
    )]
    #[serde(skip)]
    pub encoding: Option<MemcmpEncoding>,
}

impl Memcmp {
    pub fn new(offset: usize, encoded_bytes: MemcmpEncodedBytes) -> Self {
        Self {
            offset,
            bytes: encoded_bytes,
            encoding: None,
        }
    }

    pub fn new_raw_bytes(offset: usize, bytes: Vec<u8>) -> Self {
        Self {
            offset,
            bytes: MemcmpEncodedBytes::Bytes(bytes),
            encoding: None,
        }
    }

    pub fn new_base58_encoded(offset: usize, bytes: &[u8]) -> Self {
        Self {
            offset,
            bytes: MemcmpEncodedBytes::Base58(bs58::encode(bytes).into_string()),
            encoding: None,
        }
    }

    pub fn bytes(&self) -> Option<Cow<Vec<u8>>> {
        use MemcmpEncodedBytes::*;
        match &self.bytes {
            Binary(bytes) | Base58(bytes) => bs58::decode(bytes).into_vec().ok().map(Cow::Owned),
            Base64(bytes) => base64::decode(bytes).ok().map(Cow::Owned),
            Bytes(bytes) => Some(Cow::Borrowed(bytes)),
        }
    }

    pub fn convert_to_raw_bytes(&mut self) -> Result<(), RpcFilterError> {
        use MemcmpEncodedBytes::*;
        match &self.bytes {
            Binary(bytes) | Base58(bytes) => {
                let bytes = bs58::decode(bytes).into_vec()?;
                self.bytes = Bytes(bytes);
                Ok(())
            }
            Base64(bytes) => {
                let bytes = base64::decode(bytes)?;
                self.bytes = Bytes(bytes);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn bytes_match(&self, data: &[u8]) -> bool {
        match self.bytes() {
            Some(bytes) => {
                if self.offset > data.len() {
                    return false;
                }
                if data[self.offset..].len() < bytes.len() {
                    return false;
                }
                data[self.offset..self.offset + bytes.len()] == bytes[..]
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        const_format::formatcp,
        serde_json::{json, Value},
    };

    #[test]
    fn test_worst_case_encoded_tx_goldens() {
        let ff_data = vec![0xffu8; MAX_DATA_SIZE];
        let data58 = bs58::encode(&ff_data).into_string();
        assert_eq!(data58.len(), MAX_DATA_BASE58_SIZE);
        let data64 = base64::encode(&ff_data);
        assert_eq!(data64.len(), MAX_DATA_BASE64_SIZE);
    }

    #[test]
    fn test_bytes_match() {
        let data = vec![1, 2, 3, 4, 5];

        // Exact match of data succeeds
        assert!(Memcmp {
            offset: 0,
            bytes: MemcmpEncodedBytes::Base58(bs58::encode(vec![1, 2, 3, 4, 5]).into_string()),
            encoding: None,
        }
        .bytes_match(&data));

        // Partial match of data succeeds
        assert!(Memcmp {
            offset: 0,
            bytes: MemcmpEncodedBytes::Base58(bs58::encode(vec![1, 2]).into_string()),
            encoding: None,
        }
        .bytes_match(&data));

        // Offset partial match of data succeeds
        assert!(Memcmp {
            offset: 2,
            bytes: MemcmpEncodedBytes::Base58(bs58::encode(vec![3, 4]).into_string()),
            encoding: None,
        }
        .bytes_match(&data));

        // Incorrect partial match of data fails
        assert!(!Memcmp {
            offset: 0,
            bytes: MemcmpEncodedBytes::Base58(bs58::encode(vec![2]).into_string()),
            encoding: None,
        }
        .bytes_match(&data));

        // Bytes overrun data fails
        assert!(!Memcmp {
            offset: 2,
            bytes: MemcmpEncodedBytes::Base58(bs58::encode(vec![3, 4, 5, 6]).into_string()),
            encoding: None,
        }
        .bytes_match(&data));

        // Offset outside data fails
        assert!(!Memcmp {
            offset: 6,
            bytes: MemcmpEncodedBytes::Base58(bs58::encode(vec![5]).into_string()),
            encoding: None,
        }
        .bytes_match(&data));

        // Invalid base-58 fails
        assert!(!Memcmp {
            offset: 0,
            bytes: MemcmpEncodedBytes::Base58("III".to_string()),
            encoding: None,
        }
        .bytes_match(&data));
    }

    #[test]
    fn test_verify_memcmp() {
        let base58_bytes = "\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111";
        assert_eq!(base58_bytes.len(), 128);
        assert_eq!(
            RpcFilterType::Memcmp(Memcmp {
                offset: 0,
                bytes: MemcmpEncodedBytes::Base58(base58_bytes.to_string()),
                encoding: None,
            })
            .verify(),
            Ok(())
        );

        let base58_bytes = "\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111\
            1";
        assert_eq!(base58_bytes.len(), 129);
        assert_eq!(
            RpcFilterType::Memcmp(Memcmp {
                offset: 0,
                bytes: MemcmpEncodedBytes::Base58(base58_bytes.to_string()),
                encoding: None,
            })
            .verify(),
            Err(RpcFilterError::DataTooLarge)
        );
    }

    const BASE58_STR: &str = "Bpf4ERpEvSFmCSTNh1PzTWTkALrKXvMXEdthxHuwCQcf";
    const BASE64_STR: &str = "oMoycDvJzrjQpCfukbO4VW/FLGLfnbqBEc9KUEVgj2g=";
    const BYTES: [u8; 4] = [0, 1, 2, 3];
    const OFFSET: usize = 42;
    const DEFAULT_ENCODING_FILTER: &str =
        formatcp!(r#"{{"bytes":"{BASE58_STR}","offset":{OFFSET}}}"#);
    const BINARY_FILTER: &str =
        formatcp!(r#"{{"bytes":"{BASE58_STR}","offset":{OFFSET},"encoding":"binary"}}"#);
    const BASE58_FILTER: &str =
        formatcp!(r#"{{"bytes":"{BASE58_STR}","offset":{OFFSET},"encoding":"base58"}}"#);
    const BASE64_FILTER: &str =
        formatcp!(r#"{{"bytes":"{BASE64_STR}","offset":{OFFSET},"encoding":"base64"}}"#);
    const BYTES_FILTER: &str =
        formatcp!(r#"{{"bytes":[0, 1, 2, 3],"offset":{OFFSET},"encoding":null}}"#);
    const BYTES_FILTER_WITH_ENCODING: &str =
        formatcp!(r#"{{"bytes":[0, 1, 2, 3],"offset":{OFFSET},"encoding":"bytes"}}"#);

    #[test]
    fn test_filter_deserialize() {
        // Base58 is the default encoding
        let default: Memcmp = serde_json::from_str(DEFAULT_ENCODING_FILTER).unwrap();
        assert_eq!(
            default,
            Memcmp {
                offset: OFFSET,
                bytes: MemcmpEncodedBytes::Base58(BASE58_STR.to_string()),
                encoding: None,
            }
        );

        // Binary input is deserialized as Base58
        let binary: Memcmp = serde_json::from_str(BINARY_FILTER).unwrap();
        assert_eq!(
            binary,
            Memcmp {
                offset: OFFSET,
                bytes: MemcmpEncodedBytes::Base58(BASE58_STR.to_string()),
                encoding: None,
            }
        );

        // Base58 input
        let base58_filter: Memcmp = serde_json::from_str(BASE58_FILTER).unwrap();
        assert_eq!(
            base58_filter,
            Memcmp {
                offset: OFFSET,
                bytes: MemcmpEncodedBytes::Base58(BASE58_STR.to_string()),
                encoding: None,
            }
        );

        // Base64 input
        let base64_filter: Memcmp = serde_json::from_str(BASE64_FILTER).unwrap();
        assert_eq!(
            base64_filter,
            Memcmp {
                offset: OFFSET,
                bytes: MemcmpEncodedBytes::Base64(BASE64_STR.to_string()),
                encoding: None,
            }
        );

        // Raw bytes input
        let bytes_filter: Memcmp = serde_json::from_str(BYTES_FILTER).unwrap();
        assert_eq!(
            bytes_filter,
            Memcmp {
                offset: OFFSET,
                bytes: MemcmpEncodedBytes::Bytes(BYTES.to_vec()),
                encoding: None,
            }
        );

        let bytes_filter: Memcmp = serde_json::from_str(BYTES_FILTER_WITH_ENCODING).unwrap();
        assert_eq!(
            bytes_filter,
            Memcmp {
                offset: OFFSET,
                bytes: MemcmpEncodedBytes::Bytes(BYTES.to_vec()),
                encoding: None,
            }
        );
    }

    #[test]
    fn test_filter_serialize() {
        // Binary variants
        let binary_default = Memcmp {
            offset: OFFSET,
            bytes: MemcmpEncodedBytes::Binary(BASE58_STR.to_string()),
            encoding: None,
        };
        let serialized_json = json!(binary_default);
        assert_eq!(
            serialized_json,
            serde_json::from_str::<Value>(BINARY_FILTER).unwrap()
        );

        let binary_default = Memcmp {
            offset: OFFSET,
            bytes: MemcmpEncodedBytes::Binary(BASE58_STR.to_string()),
            encoding: Some(MemcmpEncoding::Binary),
        };
        let serialized_json = json!(binary_default);
        assert_eq!(
            serialized_json,
            serde_json::from_str::<Value>(BINARY_FILTER).unwrap()
        );

        let binary_base58 = Memcmp {
            offset: OFFSET,
            bytes: MemcmpEncodedBytes::Base58(BASE58_STR.to_string()),
            encoding: Some(MemcmpEncoding::Binary),
        };
        let serialized_json = json!(binary_base58);
        assert_eq!(
            serialized_json,
            serde_json::from_str::<Value>(BASE58_FILTER).unwrap()
        );

        let binary_base64 = Memcmp {
            offset: OFFSET,
            bytes: MemcmpEncodedBytes::Base64(BASE64_STR.to_string()),
            encoding: Some(MemcmpEncoding::Binary),
        };
        let serialized_json = json!(binary_base64);
        assert_eq!(
            serialized_json,
            serde_json::from_str::<Value>(BASE64_FILTER).unwrap()
        );

        let binary_bytes = Memcmp {
            offset: OFFSET,
            bytes: MemcmpEncodedBytes::Bytes(BYTES.to_vec()),
            encoding: Some(MemcmpEncoding::Binary),
        };
        let serialized_json = json!(binary_bytes);
        assert_eq!(
            serialized_json,
            serde_json::from_str::<Value>(BYTES_FILTER).unwrap()
        );

        // Base58
        let base58 = Memcmp {
            offset: OFFSET,
            bytes: MemcmpEncodedBytes::Base58(BASE58_STR.to_string()),
            encoding: None,
        };
        let serialized_json = json!(base58);
        assert_eq!(
            serialized_json,
            serde_json::from_str::<Value>(BASE58_FILTER).unwrap()
        );

        // Base64
        let base64 = Memcmp {
            offset: OFFSET,
            bytes: MemcmpEncodedBytes::Base64(BASE64_STR.to_string()),
            encoding: None,
        };
        let serialized_json = json!(base64);
        assert_eq!(
            serialized_json,
            serde_json::from_str::<Value>(BASE64_FILTER).unwrap()
        );

        // Bytes
        let bytes = Memcmp {
            offset: OFFSET,
            bytes: MemcmpEncodedBytes::Bytes(BYTES.to_vec()),
            encoding: None,
        };
        let serialized_json = json!(bytes);
        assert_eq!(
            serialized_json,
            serde_json::from_str::<Value>(BYTES_FILTER).unwrap()
        );
    }
}
