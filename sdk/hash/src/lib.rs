#![no_std]
#![cfg_attr(RUSTC_WITH_SPECIALIZATION, feature(min_specialization))]
#[cfg(feature = "borsh")]
use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
#[cfg(any(feature = "std", target_arch = "wasm32"))]
extern crate std;
#[cfg(feature = "bytemuck")]
use bytemuck_derive::{Pod, Zeroable};
#[cfg(feature = "serde")]
use serde_derive::{Deserialize, Serialize};
#[cfg(any(feature = "borsh", target_arch = "wasm32"))]
use std::string::ToString;
use {
    core::{convert::TryFrom, fmt, mem, str::FromStr},
    solana_sanitize::Sanitize,
};
#[cfg(target_arch = "wasm32")]
use {
    js_sys::{Array, Uint8Array},
    std::{boxed::Box, format, string::String, vec},
    wasm_bindgen::{prelude::*, JsCast},
};

/// Size of a hash in bytes.
pub const HASH_BYTES: usize = 32;
/// Maximum string length of a base58 encoded hash.
pub const MAX_BASE58_LEN: usize = 44;

/// A hash; the 32-byte output of a hashing algorithm.
///
/// This struct is used most often in `solana-sdk` and related crates to contain
/// a [SHA-256] hash, but may instead contain a [blake3] hash, as created by the
/// [`blake3`] module (and used in [`Message::hash`]).
///
/// [SHA-256]: https://en.wikipedia.org/wiki/SHA-2
/// [blake3]: https://github.com/BLAKE3-team/BLAKE3
/// [`blake3`]: crate::blake3
/// [`Message::hash`]: crate::message::Message::hash
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
#[cfg_attr(feature = "frozen-abi", derive(solana_frozen_abi_macro::AbiExample))]
#[cfg_attr(
    feature = "borsh",
    derive(BorshSerialize, BorshDeserialize),
    borsh(crate = "borsh")
)]
#[cfg_attr(all(feature = "borsh", feature = "std"), derive(BorshSchema))]
#[cfg_attr(feature = "bytemuck", derive(Pod, Zeroable))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize,))]
#[derive(Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct Hash(pub(crate) [u8; HASH_BYTES]);

impl Sanitize for Hash {}

impl From<[u8; HASH_BYTES]> for Hash {
    fn from(from: [u8; 32]) -> Self {
        Self(from)
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        &self.0[..]
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", bs58::encode(self.0).into_string())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", bs58::encode(self.0).into_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseHashError {
    WrongSize,
    Invalid,
}

#[cfg(feature = "std")]
impl std::error::Error for ParseHashError {}

impl fmt::Display for ParseHashError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ParseHashError::WrongSize => f.write_str("string decoded to wrong size for hash"),
            ParseHashError::Invalid => f.write_str("failed to decoded string to hash"),
        }
    }
}

impl FromStr for Hash {
    type Err = ParseHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() > MAX_BASE58_LEN {
            return Err(ParseHashError::WrongSize);
        }
        let bytes = bs58::decode(s)
            .into_vec()
            .map_err(|_| ParseHashError::Invalid)?;
        if bytes.len() != mem::size_of::<Hash>() {
            Err(ParseHashError::WrongSize)
        } else {
            Ok(Hash::new(&bytes))
        }
    }
}

impl Hash {
    pub fn new(hash_slice: &[u8]) -> Self {
        Hash(<[u8; HASH_BYTES]>::try_from(hash_slice).unwrap())
    }

    pub const fn new_from_array(hash_array: [u8; HASH_BYTES]) -> Self {
        Self(hash_array)
    }

    /// unique Hash for tests and benchmarks.
    pub fn new_unique() -> Self {
        use solana_atomic_u64::AtomicU64;
        static I: AtomicU64 = AtomicU64::new(1);

        let mut b = [0u8; HASH_BYTES];
        let i = I.fetch_add(1);
        b[0..8].copy_from_slice(&i.to_le_bytes());
        Self::new(&b)
    }

    pub fn to_bytes(self) -> [u8; HASH_BYTES] {
        self.0
    }
}

#[cfg(target_arch = "wasm32")]
#[allow(non_snake_case)]
#[wasm_bindgen]
impl Hash {
    /// Create a new Hash object
    ///
    /// * `value` - optional hash as a base58 encoded string, `Uint8Array`, `[number]`
    #[wasm_bindgen(constructor)]
    pub fn constructor(value: JsValue) -> Result<Hash, JsValue> {
        if let Some(base58_str) = value.as_string() {
            base58_str
                .parse::<Hash>()
                .map_err(|x| JsValue::from(x.to_string()))
        } else if let Some(uint8_array) = value.dyn_ref::<Uint8Array>() {
            Ok(Hash::new(&uint8_array.to_vec()))
        } else if let Some(array) = value.dyn_ref::<Array>() {
            let mut bytes = vec![];
            let iterator = js_sys::try_iter(&array.values())?.expect("array to be iterable");
            for x in iterator {
                let x = x?;

                if let Some(n) = x.as_f64() {
                    if n >= 0. && n <= 255. {
                        bytes.push(n as u8);
                        continue;
                    }
                }
                return Err(format!("Invalid array argument: {:?}", x).into());
            }
            Ok(Hash::new(&bytes))
        } else if value.is_undefined() {
            Ok(Hash::default())
        } else {
            Err("Unsupported argument".into())
        }
    }

    /// Return the base58 string representation of the hash
    pub fn toString(&self) -> String {
        self.to_string()
    }

    /// Checks if two `Hash`s are equal
    pub fn equals(&self, other: &Hash) -> bool {
        self == other
    }

    /// Return the `Uint8Array` representation of the hash
    pub fn toBytes(&self) -> Box<[u8]> {
        self.0.clone().into()
    }
}
